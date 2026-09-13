//! `az-tui`: a fast terminal browser for AKS namespaces, Azure Key Vault
//! secrets and Container Registry images.

fn main() {
    if let Err(error) = az_tui::run::run() {
        eprintln!("error: {error:#}");
        std::process::exit(1);
    }
}
