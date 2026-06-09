#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod commands;

fn main() {
    tauri::Builder::default()
        .manage(commands::AppState::new())
        .invoke_handler(tauri::generate_handler![
            commands::add_files,
            commands::add_folder,
            commands::add_dropped_paths,
            commands::remove_selected,
            commands::clear_list,
            commands::start_shredding,
            commands::cancel_shredding,
            commands::get_methods,
            commands::check_ssd,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
