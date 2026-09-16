//! Shared native folder selection for workspace and settings forms.

/// Keep the current value on cancellation. Existing paths seed the picker;
/// other inputs (including GitHub shorthand) fall back to the home folder.
pub fn browse(folder: &mut String) -> bool {
    let initial = crate::workspace::expand_dir(folder)
        .filter(|path| path.is_dir())
        .or_else(dirs::home_dir);
    let mut dialog = rfd::FileDialog::new().set_title("Choose a folder");
    if let Some(initial) = initial {
        dialog = dialog.set_directory(initial);
    }
    if let Some(selected) = dialog.pick_folder() {
        *folder = selected.display().to_string();
        true
    } else {
        false
    }
}
