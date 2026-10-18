//! Release builds must not allow environment variables to redirect credentials,
//! certificates, or paper output. Debug builds retain isolated QA fixtures.

#[cfg(not(debug_assertions))]
use print_bridge_lib::error::BridgeError;
use print_bridge_lib::{config, printer, tls_material};
use std::ffi::OsString;
use std::path::PathBuf;

const VARS: [&str; 4] = [
    "PROGRAMDATA",
    "GOURMELYPRINT_DATA_DIR",
    "GOURMELYPRINT_CERT_DIR",
    "GOURMELYPRINT_SINK_DIR",
];

struct Fixture {
    root: PathBuf,
    previous: Vec<(&'static str, Option<OsString>)>,
}

impl Fixture {
    fn new() -> Self {
        let root =
            std::env::temp_dir().join(format!("bridge-release-overrides-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).expect("create isolated fixture");
        let previous = VARS
            .into_iter()
            .map(|name| (name, std::env::var_os(name)))
            .collect();
        std::env::set_var("PROGRAMDATA", root.join("machine"));
        std::env::set_var("GOURMELYPRINT_DATA_DIR", root.join("redirected-data"));
        std::env::set_var("GOURMELYPRINT_CERT_DIR", root.join("redirected-certs"));
        std::env::set_var("GOURMELYPRINT_SINK_DIR", root.join("redirected-sink"));
        Self { root, previous }
    }

    fn sink(&self) -> PathBuf {
        self.root.join("redirected-sink")
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        for (name, value) in &self.previous {
            if let Some(value) = value {
                std::env::set_var(name, value);
            } else {
                std::env::remove_var(name);
            }
        }
        std::fs::remove_dir_all(&self.root).expect("remove isolated fixture");
    }
}

#[cfg(not(debug_assertions))]
#[test]
fn release_ignores_all_three_redirection_variables() {
    let fixture = Fixture::new();
    assert_eq!(
        config::data_dir(),
        Some(fixture.root.join("machine/GourmelyPrint"))
    );
    assert_eq!(
        tls_material::cert_dir(),
        Some(fixture.root.join("machine/GourmelyPrint/certs"))
    );
    let missing = format!("missing-printer-{}", uuid::Uuid::new_v4());
    assert!(matches!(
        printer::print_raw(&missing, b"ticket"),
        Err(BridgeError::PrinterNotFound(_))
    ));
    assert!(
        !fixture.sink().exists(),
        "release must not write paper to the sink"
    );
}

#[cfg(debug_assertions)]
#[test]
fn debug_retains_isolated_redirection_for_tests() {
    let fixture = Fixture::new();
    assert_eq!(
        config::data_dir(),
        Some(fixture.root.join("redirected-data"))
    );
    assert_eq!(
        tls_material::cert_dir(),
        Some(fixture.root.join("redirected-certs"))
    );
    assert_eq!(printer::print_raw("virtual", b"ticket").unwrap(), 0);
    let files: Vec<_> = std::fs::read_dir(fixture.sink())
        .expect("sink directory")
        .map(|entry| entry.expect("sink file").path())
        .collect();
    assert_eq!(files.len(), 1);
    assert_eq!(std::fs::read(&files[0]).unwrap(), b"ticket");
}
