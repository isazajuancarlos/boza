// Embebe el icono en el .exe de Windows (lo que muestra el Explorador, la barra de
// tareas y el diálogo de UAC). Solo en Windows; en otras plataformas no hace nada.
fn main() {
    #[cfg(windows)]
    {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("assets/boza.ico");
        if let Err(e) = res.compile() {
            // No abortar el build por el icono: se avisa y se sigue sin él.
            println!("cargo:warning=no se pudo embeber el icono de boza: {e}");
        }
    }
}
