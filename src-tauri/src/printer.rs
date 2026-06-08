//! Printer access via the `printers` crate.
//!
//! Wraps the OS print spooler (Windows Print Spooler on Windows,
//! CUPS on macOS/Linux) so this module stays small and any future
//! macOS/Linux port works without ifdef'ing every call.
//!
//! For Windows we send the bytes with the `RAW` datatype — i.e. no
//! spooler-side translation, the bytes hit the wire exactly as we
//! emit them, which is what ESC/POS requires.

use crate::error::{BridgeError, BridgeResult};
use printers::common::base::job::PrinterJobOptions;
use printers::common::base::printer::Printer;
use printers::common::converters::Converter;

/// Returns the printable names of every printer registered on this
/// machine.
pub fn list() -> BridgeResult<Vec<String>> {
    let printers = printers::get_printers();
    Ok(printers.into_iter().map(|p| p.name).collect())
}

/// Sends `bytes` to `printer_name` as a RAW print job. Returns the print
/// job id (or 0 if the underlying API doesn't expose one).
pub fn print_raw(printer_name: &str, bytes: &[u8]) -> BridgeResult<u32> {
    let printer = find_printer(printer_name)?;
    // RAW (no-conversion) datatype is what ESC/POS thermal printers need;
    // the spooler must pass bytes through untouched.
    let opts = PrinterJobOptions {
        name: Some("GourmelyPrint Job"),
        raw_properties: &[],
        converter: Converter::None,
    };
    printer
        .print(bytes, opts)
        .map_err(|e| BridgeError::SpoolerFailed(format!("{e:?}")))?;
    // `printers` crate doesn't expose the spooler job id; surface 0 so
    // the wire response shape stays consistent.
    Ok(0)
}

fn find_printer(name: &str) -> BridgeResult<Printer> {
    printers::get_printers()
        .into_iter()
        .find(|p| p.name == name)
        .ok_or_else(|| BridgeError::PrinterNotFound(name.to_string()))
}
