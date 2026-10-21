//! Prints the backup coverage page: every resource Thorium stores and how backup treats it
//!
//! Run it with `cargo run -p thoradm --example coverage_page`.

use thoradm::backup::coverage::Registry;

/// Prints the coverage page to stdout
fn main() {
    // render the registry as Markdown
    print!("{}", Registry::render_page());
}
