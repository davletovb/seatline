//! The fake provider binary for the runtime's tests: `seatline-fake-provider`
//! wrapped in a `main`, so the tests can start it through `CARGO_BIN_EXE_*`.

fn main() -> std::process::ExitCode {
    seatline_fake_provider::run()
}
