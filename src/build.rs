/* K1sh-M1sh Members - License MIT */
/* © 2026, K1sh-M1sh Members */

fn main() {
    if std::env::var_os("CARGO_CFG_WINDOWS").is_some() {
        println!("cargo:rerun-if-changed=images/icon.ico");
        let mut res = winresource::WindowsResource::new();
        res.set_icon("images/icon.ico");
        res.compile().expect("Не удалось вшить иконку");
    }
}