use jcp::{
    auth::{get_access_token, login},
    keychain,
};

fn main() {
    let keychain = keychain::active_keychain();
    let token = if let Some(refresh_token) = keychain.get_refresh_token().unwrap() {
        get_access_token(&refresh_token).unwrap()
    } else {
        login().unwrap()
    };
    eprintln!("=== Authentication Successful ===\n");
    eprintln!("Access Token:\n  {}\n", token);
}
