//! Printer access via the `printers` crate.
//!
//! Wraps the OS print spooler (Windows Print Spooler on Windows,
//! CUPS on macOS/Linux) so this module stays small and any future
//! macOS/Linux port works without ifdef'ing every call.
//!
//! For Windows we send the bytes with the `RAW` datatype — i.e. no
//! spooler-side translation, the bytes hit the wire exactly as we
//! emit them, which is what ESC/POS requires.

use std::path::PathBuf;

use crate::error::{BridgeError, BridgeResult};
use printers::common::base::job::PrinterJobOptions;
use printers::common::base::printer::Printer;
use printers::common::converters::Converter;

/// Env var that redirects every print job to a file instead of the spooler.
///
/// This is the QA and support hatch, and it earns its place: the relay has to
/// be provable end to end — pair, connect, receive, print, ack — and a CI
/// runner has no thermal printer, nor does the machine of whoever is debugging
/// a customer's ticket at two in the morning. Writing the ESC/POS bytes to a
/// file exercises the same path the spooler does, up to the spooler itself.
///
/// Off unless the variable is set, and every job logs a warning saying where
/// the paper went — an install that silently swallowed tickets into a folder
/// would be indistinguishable from a printer that works.
const SINK_ENV: &str = "GOURMELYPRINT_SINK_DIR";

fn sink_dir() -> Option<PathBuf> {
    std::env::var_os(SINK_ENV)
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
}

/// Returns the printable names of every printer registered on this
/// machine.
pub fn list() -> BridgeResult<Vec<String>> {
    let printers = printers::get_printers();
    Ok(printers.into_iter().map(|p| p.name).collect())
}

/// Sends `bytes` to `printer_name` as a RAW print job. Returns the print
/// job id (or 0 if the underlying API doesn't expose one).
pub fn print_raw(printer_name: &str, bytes: &[u8]) -> BridgeResult<u32> {
    if let Some(dir) = sink_dir() {
        return print_to_sink(&dir, printer_name, bytes);
    }
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

/// Writes the raw ESC/POS bytes to `<dir>/<printer>-<timestamp>.escpos`.
///
/// The bytes land untouched, so the file can be piped at a real printer later
/// (`copy /b file \\host\printer`) or read with a hex viewer to check that the
/// cut command and the logo raster survived the round trip.
fn print_to_sink(dir: &PathBuf, printer_name: &str, bytes: &[u8]) -> BridgeResult<u32> {
    use std::time::{SystemTime, UNIX_EPOCH};
    std::fs::create_dir_all(dir)?;
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let safe: String = printer_name
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    let path = dir.join(format!("{safe}-{stamp}.escpos"));
    std::fs::write(&path, bytes)?;
    tracing::warn!(
        path = %path.display(),
        printer = printer_name,
        bytes = bytes.len(),
        "{SINK_ENV} is set: the job went to a FILE, not to the printer"
    );
    Ok(0)
}

/// Canonical ESC/POS test ticket — used by both the tray menu and the
/// WS `test` op so the two flows print byte-identical pages. Kept here
/// so anything that wants "a known-good print job for QA" reaches for
/// the same template.
pub fn test_ticket_bytes() -> Vec<u8> {
    let mut b = Vec::with_capacity(128);
    // ESC @  init
    // ESC a 0x01  center
    // ESC ! 0x30  double height + width
    b.extend_from_slice(b"\x1b@\x1ba\x01\x1b!\x30");
    b.extend_from_slice("GOURMELYPRINT\n".as_bytes());
    // ESC ! 0x00  normal size
    b.extend_from_slice(b"\x1b!\x00");
    b.extend_from_slice("Test desde el bridge\n\n".as_bytes());
    // LF×4 + GS V 0x01  feed + partial cut
    b.extend_from_slice(b"\x0a\x0a\x0a\x0a\x1dV\x01");
    b
}
