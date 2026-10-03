//! The fake provider the harness installs under a real CLI's name.

fn main() -> std::process::ExitCode {
    seatline_fake_provider::run()
}
