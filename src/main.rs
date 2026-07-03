fn main() {
    println!("slack-codex bootstrap");
}

#[cfg(test)]
mod tests {
    #[test]
    fn package_name_is_stable() {
        assert_eq!(env!("CARGO_PKG_NAME"), "slack-codex");
    }
}
