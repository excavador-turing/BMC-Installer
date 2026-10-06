//! This is the main binary that runs when the installer is used on the SD Card installation image.
//!
//! It runs in the initramfs environment, and is the only binary, which means:
//! 1. Its runtime path is `/init`.
//! 2. Exiting will panic the kernel. This is fine in principle, but may bother the user, so errors
//!    and successful completion alike should end with telling the user to remove the card and
//!    reset the BMC, then waiting.
//! 3. There are no other binaries to call into. As tempted as we may be, we can't rely on
//!    subprocesses to do any of the work. This binary needs to be self-contained.
//! 4. The filesystem starts empty. Essential mountpoints like `/proc` and `/sys` need to be
//!    established before any meaningful work can be done.
use bmc_installer::install::SettingsPlan;
use bmc_installer::turing_pi::{
    install_mode_from_cmdline, led, read_from_sdcard, setup_initramfs, upgrade_bmc, wait_forever,
    Confirmation,
};
use std::{
    io,
    sync::{self, atomic},
    thread,
    time::Duration,
};

const INTRO: &str = "\
This utility will perform a fresh installation of the Turing Pi 2 BMC firmware.
";

const SETTINGS_KEPT: &str = "\
Settings stored on this board (root password, TLS certificate, network and node
settings) will be KEPT. To erase them instead and restore factory defaults, type
'ERASE' at the below prompt.
";

const SETTINGS_ERASED: &str = "\
This will ERASE ALL USER DATA stored on the Turing Pi 2 BMC, thus restoring
back to factory defaults. Do NOT proceed unless you have first backed up any
files that you care about!
";

const HOW_TO_CONFIRM: &str = "\
If you wish to confirm the operation and proceed, either:
1) Type 'CONFIRM' at the below prompt
2) Press one of the front panel buttons (POWER or RESET), or the KEY1 button on
   the Turing Pi 2 board itself, three times in a row

If you are here in error, please remove the microSD card from the Turing Pi 2
board and reset the BMC.
";

/// The text shown before the prompt, saying truthfully what is about to happen to the settings.
fn instructions(plan: &SettingsPlan) -> String {
    let settings = match plan {
        SettingsPlan::Keep(_) => SETTINGS_KEPT.to_string(),
        SettingsPlan::Reset => format!("A factory reset was requested.\n\n{SETTINGS_ERASED}"),
        SettingsPlan::CannotKeep(reason) => {
            format!("Settings cannot be kept: {reason}.\n\n{SETTINGS_ERASED}")
        }
    };
    format!("{INTRO}\n{settings}\n{HOW_TO_CONFIRM}")
}

const KEYS_EVDEV_PATH: &str = "/dev/input/event0";

/// Wait until the user confirms the installation operation, through either the serial prompt or
/// by pressing a GPIO key multiple times. Typing `ERASE` instead asks for a factory reset; the
/// keys can only confirm what the prompt said.
///
/// This spawns one thread each, for both methods.
fn wait_for_confirmation(offer_erase: bool) -> Confirmation {
    let signals = sync::Arc::new((atomic::AtomicBool::new(false), thread::current()));
    let signals_1 = signals.clone();
    let signals_2 = signals.clone();
    let mut threads = [
        Some(thread::spawn(move || {
            let (stop_flag, main_thread) = &*signals_1;
            let ret = confirm_prompt(stop_flag, offer_erase);
            main_thread.unpark();
            ret
        })),
        Some(thread::spawn(move || {
            let (stop_flag, main_thread) = &*signals_2;
            let ret = confirm_keypress(stop_flag);
            main_thread.unpark();
            ret
        })),
    ];

    thread::park();

    loop {
        for thread in &mut threads {
            if thread.as_ref().is_some_and(|thread| thread.is_finished()) {
                let ret = thread
                    .take()
                    .unwrap()
                    .join()
                    .expect("thread should not panic");
                if let Some(confirmation) = ret {
                    signals.0.store(true, atomic::Ordering::Relaxed);
                    return confirmation;
                }
                thread::park();
            }
        }
    }
}

/// Repeatedly nag the user to type "CONFIRM" (or "ERASE", for a factory reset)
///
/// "ERASE" is always accepted; it is only mentioned when the settings would otherwise be kept.
fn confirm_prompt(stop_flag: &atomic::AtomicBool, offer_erase: bool) -> Option<Confirmation> {
    const CONFIRM_KEYWORD: &str = "CONFIRM";
    const ERASE_KEYWORD: &str = "ERASE";

    let mut input = String::new();
    loop {
        if stop_flag.load(atomic::Ordering::Relaxed) {
            return None;
        }
        if offer_erase {
            eprint!(
                "Type \"{CONFIRM_KEYWORD}\" to continue keeping settings, \
                 or \"{ERASE_KEYWORD}\" to erase them: "
            );
        } else {
            eprint!("Type \"{CONFIRM_KEYWORD}\" to continue: ");
        }
        input.clear();
        match io::stdin().read_line(&mut input) {
            Ok(_) if input.trim_end() == CONFIRM_KEYWORD => return Some(Confirmation::Proceed),
            Ok(_) if input.trim_end() == ERASE_KEYWORD => return Some(Confirmation::FactoryReset),
            Ok(0) => return None,
            _ => continue,
        };
    }
}

/// Monitor for a key being pressed three times
fn confirm_keypress(stop_flag: &atomic::AtomicBool) -> Option<Confirmation> {
    const KEYPRESS_TIMEOUT: Duration = Duration::from_millis(500);
    const KEYPRESS_TIMES: u8 = 3;

    let mut device = match evdev::raw_stream::RawDevice::open(KEYS_EVDEV_PATH) {
        Ok(device) => device,
        Err(_) => return None,
    };

    let mut last_key = None;
    let mut last_time = None;
    let mut times_pressed = 0;

    loop {
        if stop_flag.load(atomic::Ordering::Relaxed) {
            return None;
        }

        let events = match device.fetch_events() {
            Ok(events) => events,
            Err(_) => return None,
        };

        for event in events {
            // Only follow key events
            let key = match event.kind() {
                evdev::InputEventKind::Key(key) => key,
                _ => continue,
            };

            // All keypresses have to be the same key; start over if the user switched keys
            if last_key != Some(key) {
                last_key = Some(key);
                times_pressed = 0;
            }

            // Only handle key-up events past this point
            if event.value() != 0 {
                continue;
            }

            // Determine how long has passed since the last key-up event (or None)
            let timestamp = event.timestamp();
            let time_elapsed = last_time
                .replace(timestamp)
                .and_then(|x| timestamp.duration_since(x).ok());

            // If past the timeout (or None), start over
            if time_elapsed.is_none_or(|x| x > KEYPRESS_TIMEOUT) {
                times_pressed = 0;
            }

            times_pressed += 1;
            if times_pressed >= KEYPRESS_TIMES {
                return Some(Confirmation::Proceed);
            }
        }
    }
}

/// The main SD Card installation program.
///
/// This function must never return.
fn main() -> ! {
    // Set up the LED blinking thread, in order to indicate further init errors
    let led_tx = led::led_blink_thread();

    let result = setup_initramfs().and_then(|_| read_from_sdcard());

    let Ok((bootloader, rootfs)) = result else {
        eprintln!(
            "[-] The installer could not initialize properly:\n{}",
            result.unwrap_err()
        );
        let _ = led_tx.send(led::LED_ERROR);
        wait_forever();
    };

    let mode = install_mode_from_cmdline();

    let confirm = |plan: &SettingsPlan| {
        eprintln!("{}", instructions(plan));
        wait_for_confirmation(plan.keeps_settings())
    };

    if let Err(error) = upgrade_bmc(rootfs, bootloader, mode, confirm, led_tx.clone()) {
        eprintln!("[-] Installation error:\n{error}");
        let _ = led_tx.send(led::LED_ERROR);
    } else {
        eprintln!("[+] DONE: Please remove the microSD card and reset the BMC.");
    }

    wait_forever()
}
