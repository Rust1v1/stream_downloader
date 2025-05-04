use regex::Regex;
use reqwest::Client;
use serde_json::Value;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let room_regex: Regex =
        Regex::new(r#"window\.initialRoomDossier\s*=\s*("(?:\\.|[^\\"])*")"#)?;

    let client = Client::new();
    let html = client
        .get("https://user_url")
        .send()
        .await
        .unwrap();
    let html_text = html.text().await?;

    let json_str = room_regex
        .captures(&html_text)
        .and_then(|caps| caps.get(1))
        .unwrap()
        .as_str();

    let json_obj: Value = serde_json::from_str(&json_str)?;

    Ok(())
}

#[cfg(test)]
#[test]
fn online_user_test() {
    use regex::Regex;
    use serde_json::Value;
    use std::fs;

    let online_user_html: String =
        fs::read_to_string("./html/online.html").expect("Could Not Read HTML File");

    let room_regex: Regex =
        Regex::new(r#"window\.initialRoomDossier\s*=\s*("(?:\\.|[^\\"])*")"#).expect("Invalid Room Regex");

    let dossier_json_str: &str = room_regex
        .captures(&online_user_html)
        .and_then(|captures| captures.get(1))
        .expect("No Valid Regex Found")
        .as_str();
    let json_obj: Value =
        serde_json::from_str(&dossier_json_str).expect("Invalid JSON Object Received");

    let dossier_obj: Value = serde_json::from_str(&json_obj.as_str().unwrap()).expect("Invalid Dossier JSON");
    assert!(dossier_obj.get("hls_source").is_some());

    // NOTE: These strings are all returned as "some_text" with escaped quotations. So analyzing it needs \"some_text\"
    let m3u8_link: String = dossier_obj.get("hls_source").unwrap().to_string();
    println!("{}", m3u8_link);

    assert!(m3u8_link != String::from("\"\""));
    assert!(m3u8_link.ends_with(".m3u8\""));
}

#[test]
fn offline_user_test() {
    use regex::Regex;
    use serde_json::Value;
    use std::fs;

    let online_user_html: String =
        fs::read_to_string("./html/offline.html").expect("Could Not Read HTML File");

    let room_regex: Regex =
        Regex::new(r#"window\.initialRoomDossier\s*=\s*("(?:\\.|[^\\"])*")"#).expect("Invalid Room Regex");

    let dossier_json_str: &str = room_regex
        .captures(&online_user_html)
        .and_then(|captures| captures.get(1))
        .expect("No Valid Regex Found")
        .as_str();
    let json_obj: Value =
        serde_json::from_str(&dossier_json_str).expect("Invalid JSON Object Received");

    let dossier_obj: Value = serde_json::from_str(&json_obj.as_str().unwrap()).expect("Invalid Dossier JSON");
    assert!(dossier_obj.get("hls_source").is_some());

    let m3u8_link: String = dossier_obj.get("hls_source").unwrap().to_string();
    println!("{}", m3u8_link);

    assert!(m3u8_link == String::from("\"\""));
    assert!(!m3u8_link.ends_with(".m3u8\""));

}
