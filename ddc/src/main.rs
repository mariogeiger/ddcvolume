//! Read or change the speaker volume of external displays over DDC/CI.
//!
//!     ddcvol        print the volume
//!     ddcvol 30     set it to 30
//!     ddcvol +5     raise it by 5
//!     ddcvol -5     lower it by 5

use ddc::VOLUME;
use std::process::ExitCode;

enum Request {
    Show,
    Absolute(i32),
    Relative(i32),
}

fn parse(arg: Option<&str>) -> Result<Request, String> {
    let Some(arg) = arg else { return Ok(Request::Show) };
    let number = |s: &str| s.parse::<i32>().map_err(|_| format!("not a volume: {arg}"));
    match arg.as_bytes().first() {
        Some(b'+' | b'-') => Ok(Request::Relative(number(arg)?)),
        _ => Ok(Request::Absolute(number(arg)?)),
    }
}

fn run(request: &Request) -> Result<(), String> {
    let displays = ddc::displays();
    if displays.is_empty() {
        return Err("no external display found".into());
    }
    for display in &displays {
        let vcp = display.get(VOLUME)?;
        let target = match *request {
            Request::Show => {
                println!("{}/{}", vcp.current, vcp.max);
                continue;
            }
            Request::Absolute(v) => v,
            Request::Relative(d) => vcp.current as i32 + d,
        };
        let target = target.clamp(0, vcp.max as i32) as u16;
        display.set(VOLUME, target)?;
        println!("{target}/{}", vcp.max);
    }
    Ok(())
}

fn main() -> ExitCode {
    let arg = std::env::args().nth(1);
    match parse(arg.as_deref()).and_then(|request| run(&request)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("ddcvol: {e}");
            ExitCode::FAILURE
        }
    }
}
