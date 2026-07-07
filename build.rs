fn main() {
    #[cfg(windows)]
    {
        if let Err(error) = winresource::WindowsResource::new()
            .set_icon("resources/logo.ico")
            .compile()
        {
            panic!("failed to compile Windows resources: {error}");
        }
    }
}
