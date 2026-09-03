fn main() {
    tauri_build::try_build(tauri_build::Attributes::new().app_manifest(
        tauri_build::AppManifest::new().commands(&[
            "desktop_managed_local",
            "desktop_update_status",
            "desktop_check_for_update",
            "desktop_install_update",
        ]),
    ))
    .expect("building Tauri permissions");
}
