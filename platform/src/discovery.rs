//! Policy for provider executable discovery. The reusable directory search
//! lives in `seatline-core`; adapters use this entry point so every installed
//! provider honors the same override, including hermetic tests.

use seatline_core::discovery::SearchPath;

use crate::layout::Layout;

/// Search locations for an installed adapter, with the application's
/// override: the directories, in `PATH` form, that the variable
/// [`Layout::search_path_variable`] names replace the usual lookup.
pub fn installed(layout: &Layout) -> SearchPath {
    SearchPath::from_env(&layout.search_path_variable())
}

#[cfg(test)]
mod tests {
    use super::*;
    use seatline_core::turn::Namespace;

    #[test]
    fn the_override_variable_is_named_after_the_namespace() {
        let layout = Layout::new(Namespace::fixed("app").unwrap());
        assert_eq!(layout.search_path_variable(), "APP_PROVIDER_PATH");
        // A namespace with a hyphen still names a legal variable.
        let layout = Layout::new(Namespace::fixed("my-app").unwrap());
        assert_eq!(layout.search_path_variable(), "MY_APP_PROVIDER_PATH");
        // Building the search path never panics, whatever is set.
        let _ = installed(&layout);
    }
}
