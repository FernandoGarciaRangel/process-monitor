// Embute o ícone e os metadados de versão no .exe (o que o Explorer e o
// instalador mostram). O ícone da janela vem de assets/app.png, em main.rs.
fn main() {
    println!("cargo:rerun-if-changed=assets/app.ico");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("assets/app.ico")
            .set("ProductName", "Process Monitor")
            .set("FileDescription", "Process Monitor")
            .set("CompanyName", "Fernando Garcia")
            .set("LegalCopyright", "© 2026 Fernando Garcia");
        res.compile().expect("falha ao embutir recursos do Windows");
    }
}
