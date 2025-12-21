use http::Request;

pub fn http_client() {
    println!("Hello, world!");

    let request = Request::builder()
        .uri("https://www.rust-lang.org/")
        .header("User-Agent", "awesome/1.0")
        .body(())
        .unwrap();

    let headers = request.headers();
    let body = request.body();

    println!("[] => {:?}\n{:?}\n{:?}", request, headers, body);
}
