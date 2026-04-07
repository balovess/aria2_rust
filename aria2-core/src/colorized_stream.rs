use colored::Colorize;
use std::io::{self, Write};

pub struct ColorizedStream {
    use_color: bool,
}

impl ColorizedStream {
    pub fn new(use_color: bool) -> Self {
        ColorizedStream { use_color }
    }

    pub fn print_error(&self, message: &str) {
        if self.use_color {
            eprintln!("{}", message.red().bold());
        } else {
            eprintln!("ERROR: {}", message);
        }
    }

    pub fn print_warning(&self, message: &str) {
        if self.use_color {
            println!("{}", message.yellow());
        } else {
            println!("WARNING: {}", message);
        }
    }

    pub fn print_success(&self, message: &str) {
        if self.use_color {
            println!("{}", message.green());
        } else {
            println!("{}", message);
        }
    }

    pub fn print_info(&self, message: &str) {
        if self.use_color {
            println!("[INFO] {}", message.cyan());
        } else {
            println!("[INFO] {}", message);
        }
    }

    pub fn print_progress(&self, message: &str) {
        if self.use_color {
            print!("\r{}", message.blue());
        } else {
            print!("\r{}", message);
        }
        let _ = io::stdout().flush();
    }

    pub fn is_color_supported() -> bool {
        atty::is(atty::Stream::Stdout)
    }
}
