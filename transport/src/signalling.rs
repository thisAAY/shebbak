use anyhow::{anyhow, Context, Result};
use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;

/// One-shot signalling server: blocks until a single POST /offer arrives.
/// Returns the parsed offer and a responder to complete the HTTP exchange.
///
/// Binds `0.0.0.0` (all interfaces), not just loopback: M2 sharing happens
/// over LAN, so a client on another machine must be able to reach this port.
pub fn serve_one_offer(port: u16) -> Result<(RTCSessionDescription, OfferResponder)> {
    let server = tiny_http::Server::http(("0.0.0.0", port))
        .map_err(|e| anyhow!("bind signalling port {port}: {e}"))?;
    loop {
        let mut request = server.recv().context("accept signalling request")?;
        if request.method() != &tiny_http::Method::Post || request.url() != "/offer" {
            let _ = request.respond(tiny_http::Response::empty(404));
            continue;
        }
        let mut body = String::new();
        request
            .as_reader()
            .read_to_string(&mut body)
            .context("read offer body")?;
        let offer: RTCSessionDescription =
            serde_json::from_str(&body).context("parse offer SDP JSON")?;
        return Ok((
            offer,
            OfferResponder {
                request,
                _server: server,
            },
        ));
    }
}

pub struct OfferResponder {
    request: tiny_http::Request,
    _server: tiny_http::Server,
}

impl OfferResponder {
    pub fn respond(self, answer: &RTCSessionDescription) -> Result<()> {
        let json = serde_json::to_string(answer)?;
        self.request
            .respond(
                tiny_http::Response::from_string(json).with_header(
                    "Content-Type: application/json"
                        .parse::<tiny_http::Header>()
                        .unwrap(),
                ),
            )
            .context("respond with answer")?;
        Ok(())
    }
}

/// Client side: POST the offer, get the answer.
pub fn post_offer(url: &str, offer: &RTCSessionDescription) -> Result<RTCSessionDescription> {
    let body = serde_json::to_string(offer)?;
    let resp = ureq::post(url)
        .set("Content-Type", "application/json")
        .send_string(&body)
        .map_err(|e| anyhow!("signalling POST failed: {e}"))?;
    let answer: RTCSessionDescription =
        serde_json::from_reader(resp.into_reader()).context("parse answer SDP JSON")?;
    Ok(answer)
}
