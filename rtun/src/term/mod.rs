pub mod async_input;

pub fn get_shell_program() -> String {
    // "bash".to_string()
    std::env::var("SHELL").unwrap_or("bash".to_string())
}
