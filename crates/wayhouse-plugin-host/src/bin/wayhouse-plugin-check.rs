//! `wayhouse-plugin-check <module.wasm>`: run the conformance check on a built plugin.
//! Exits 0 when the host would accept and run it, 1 on problems, 2 on usage errors.

use std::process::ExitCode;

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    let (Some(path), None) = (args.next(), args.next()) else {
        eprintln!("usage: wayhouse-plugin-check <module.wasm>");
        return ExitCode::from(2);
    };
    let bytes = match std::fs::read(&path) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("{path}: {e}");
            return ExitCode::from(2);
        }
    };
    let report = wayhouse_plugin_host::conformance::check(&bytes);
    if let Some(info) = &report.info {
        println!("abi: {}", info.abi);
        println!("sha256: {}", info.sha256);
        println!("declared: {:?}", info.caps);
    }
    for (name, fx) in [("init", &report.init), ("on_timer", &report.on_timer)] {
        if let Some(fx) = fx {
            println!(
                "{name}: ok (log used: {}, state used: {}, {} log lines, {} state writes)",
                fx.used_log,
                fx.used_state,
                fx.logs.len(),
                fx.state_puts.len()
            );
        }
    }
    for p in &report.problems {
        eprintln!("problem: {p}");
    }
    if report.ok() {
        println!("conformance: ok");
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}
