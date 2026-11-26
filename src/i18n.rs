/* K1sh-M1sh Members - License MIT */
/* © 2026, K1sh-M1sh Members */

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Language {
    En,
    Ru,
}

impl Default for Language {
    fn default() -> Self {
        Self::from_system()
    }
}

impl Language {
    pub fn from_system() -> Self {
        if let Some(locale) = sys_locale::get_locale() {
            if locale.to_lowercase().starts_with("ru") {
                return Language::Ru;
            }
        }
        Language::En
    }

    pub fn label(&self) -> &'static str {
        match self {
            Language::En => "English",
            Language::Ru => "Русский",
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct Loc {
    pub ready: String,
    pub well_done: String,
    pub settings_auto_save: String,
    pub settings_theme: String,
    pub settings_language: String,
    pub big_file_error: String,
    pub err_to_open: String,
    pub cannot_be_save: String,
    pub save: String,
    pub err_save: String,
    pub err_create_a_file: String,
    pub uncorrect_name: String,
    pub err_to_create: String,
    pub err_to_start: String,
    pub err_unknown_type: String,
    pub successfully_start: String,
    pub unsuccessfully_start: String,
    pub git_ready: String,
    pub git_error: String,
    pub err_start_terminal: String,
    pub new_file: String,
    pub new_folder: String,
    pub folder_not_open: String,
    pub open_folder: String,
    pub update: String,
    pub git_turn_off: String,
    pub open_a_folder: String,
    pub trust_folder: String,
    pub loading: String,
    pub not_git_repo: String,
    pub init_repo: String,
    pub branch: String,
    pub new_branch: String,
    pub create: String,
    pub commit_msg: String,
    pub history: String,
    pub in_index: String,
    pub remove_from_index: String,
    pub changes: String,
    pub cancel_changes: String,
    pub unnamed: String,
    pub limited_mode: String,
    pub file: String,
    pub open_file: String,
    pub btn_settings: String,
    pub btn_store: String,
    pub view: String,
    pub sidebar: String,
    pub terminal: String,
    pub output: String,
    pub start_bar: String,
    pub start_btn: String,
    pub compil: String,
    pub reference: String,
    pub about: String,
    pub report_a_bug: String,
    pub spaces: String,
    pub output_text: String,
    pub clearning_btn: String,
    pub hide_btn: String,
    pub terminal_text: String,
    pub err_trust_terminal: String,
    pub welcome_cap: String,
    pub welcome_text: String,
    pub about_text_1: String,
    pub about_text_2: String,
    pub about_text_3: String,
    pub unsaved: String,
    pub unsaved_text: String,
    pub save_unsaved_btn: String,
    pub unsave_unsaved_btn: String,
    pub cancel_btn: String,
    pub store_text: String,
    pub appear_leter: String,
    pub search_replace: String,
    pub search: String,
    pub replace: String,
    pub find_below: String,
    pub found: String,
    pub nothing_found: String,
    pub replace_btn: String,
    pub replace_all: String,
    pub replaced: String,
    pub go_to_line: String,
    pub line: String,
    pub go_btn: String,
    pub unsaved_confirm: String,
    pub unsave_confirm: String,
    pub cancel: String,
    pub unsaved_scripts_name: String,
    pub btn_yes: String,
    pub btn_no: String,
    pub trust_folder_text: String,
    pub editing: String,
    pub checkbox_subfolders: String,
    pub to_trust: String,
    pub limited_mod_btn: String,
    pub settings_window: String,
    pub user: String,
    pub project: String,
    pub settings_font_size: String,
    pub settings_tab_size: String,
    pub settings_insert_spaces: String,
    pub settings_word_wrap: String,
    pub settings_line_numbers: String,
    pub settings_indent_guides: String,
    pub terminal_shell: String,
    pub build_command: String,
    pub run_command: String,
    pub revoke_trust: String,
    pub trust_project: String,
    pub save_button_an_file: String,
    pub open_file_button_an_file: String,
    pub save_all: String,
    pub error_autosave: String
}

impl Loc {
    pub fn load(lang: Language) -> Self {
        let raw = match lang {
            Language::Ru => include_str!("../localization/ru.json"),
            Language::En => include_str!("../localization/en.json"),
        };
        serde_json::from_str(raw).expect("Invalid localization JSON")
    }
}
