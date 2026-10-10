//! whipserversrc aborts the process when webrtcbin completes ICE gathering a
//! second time for a session.
//!
//! The WHIP server signaller (gst-plugin-webrtc, `whip_signaller/imp.rs`)
//! sends the SDP answer when `ice-gathering-state` becomes `complete` and
//! takes the answer channel out of its map with
//! `.expect("SDP answer Sender needs to be valid")`. When the state reaches
//! `complete` again for the same session (webrtcbin goes back to
//! `gathering` when more candidates come in), the channel is gone and the
//! expect panics inside a GObject notify callback: the panic cannot unwind
//! into C, so the process aborts.
//!
//! Usage: whipserversrc-ice-panic-repro [bundled|unbundled|renotify] [sessions] [stun-uri]
//!
//! - `bundled`   one ICE transport per offer (control)
//! - `unbundled` audio and video without BUNDLE: one ICE transport each, so
//!               the gathering state can complete, go back to gathering and
//!               complete again
//! - `renotify`  deterministic: after the answer was sent, notify
//!               `ice-gathering-state` again, as webrtcbin does when it
//!               completes a second time
//!
//! Each session posts an offer to the signaller's `post` action (the way an
//! application serving WHIP over its own HTTP server does) and waits for the
//! answer; before the next one it deletes the session and restarts the
//! source, like a publisher reconnecting. No peer ever connects, only ICE
//! gathering runs.

use gst::glib;
use gst::prelude::*;
use std::sync::{Arc, Mutex};
use std::time::Duration;

const FINGERPRINT: &str = "CD:5B:D5:5B:28:63:41:CC:8F:C1:34:FF:DD:2B:E5:D8:\
                           D1:4F:75:41:10:8D:2B:3D:3C:E5:56:E6:B0:39:16:0B";

fn offer(bundled: bool, session: usize) -> String {
    let media = |kind: &str, pt: u32, codec: &str, mid: &str, ufrag: &str| {
        format!(
            "m={kind} 9 UDP/TLS/RTP/SAVPF {pt}\r\n\
             c=IN IP4 0.0.0.0\r\n\
             a=setup:actpass\r\n\
             a=ice-ufrag:{ufrag}\r\n\
             a=ice-pwd:apkCtBfU1SdSMhZk7K1a8SN2WLovGEIK\r\n\
             a=rtcp-mux\r\n\
             a=sendonly\r\n\
             a=mid:{mid}\r\n\
             a=fingerprint:sha-256 {FINGERPRINT}\r\n\
             a=rtpmap:{pt} {codec}\r\n"
        )
    };
    let (group, audio_ufrag) = if bundled {
        ("a=group:BUNDLE video0 audio1\r\n", "videoufrag")
    } else {
        ("", "audioufrag")
    };
    format!(
        "v=0\r\no=- {session} 0 IN IP4 0.0.0.0\r\ns=-\r\nt=0 0\r\n{group}{}{}",
        media("video", 96, "VP8/90000", "video0", "videoufrag"),
        media("audio", 111, "OPUS/48000/2", "audio1", audio_ufrag),
    )
}

fn main() {
    let mut args = std::env::args().skip(1);
    let mode = args.next().unwrap_or_else(|| "unbundled".into());
    let sessions: usize = args.next().map_or(20, |n| n.parse().expect("sessions"));
    let stun = args.next();

    gst::init().unwrap();
    gstrswebrtc::plugin_register_static().unwrap();
    println!(
        "GStreamer {}, gst-plugin-webrtc {}, mode {mode}, {sessions} sessions, stun {stun:?}",
        gst::version_string(),
        gst::Registry::get()
            .find_plugin("rswebrtc")
            .map(|p| p.version().to_string())
            .unwrap_or_default(),
    );

    let pipeline = gst::Pipeline::new();
    let src = gst::ElementFactory::make("whipserversrc")
        .property("stun-server", stun.as_deref())
        .build()
        .unwrap();
    let proxy = src.dynamic_cast_ref::<gst::ChildProxy>().unwrap();
    // No built-in HTTP server: the application calls `post` itself.
    proxy.set_child_property("signaller::host-addr", "");
    let signaller = proxy.child_by_name("signaller").unwrap();
    pipeline.add(&src).unwrap();

    let pl = pipeline.downgrade();
    src.connect_pad_added(move |_, pad| {
        let Some(pipeline) = pl.upgrade() else { return };
        let sink = gst::ElementFactory::make("fakesink")
            .property("async", false)
            .build()
            .unwrap();
        pipeline.add(&sink).unwrap();
        sink.sync_state_with_parent().unwrap();
        let _ = pad.link(&sink.static_pad("sink").unwrap());
    });

    // The webrtcbin of the current session, for `renotify`.
    let webrtcbin: Arc<Mutex<Option<gst::Element>>> = Arc::default();
    let current = webrtcbin.clone();
    signaller.connect_closure(
        "webrtcbin-ready",
        false,
        glib::closure!(move |_: &glib::Object, _session: &str, bin: &gst::Element| {
            *current.lock().unwrap() = Some(bin.clone());
        }),
    );

    pipeline.set_state(gst::State::Playing).unwrap();

    let mut previous: Option<String> = None;
    for n in 1..=sessions {
        // A publisher reconnecting: end the previous session and restart the
        // source before the next offer.
        if let Some(id) = previous.take() {
            let promise = gst::Promise::new();
            signaller.emit_by_name::<()>("delete", &[&id, &promise]);
            promise.wait();
            pipeline.set_state(gst::State::Null).unwrap();
            pipeline.set_state(gst::State::Playing).unwrap();
        }
        let promise = gst::Promise::new();
        let body = glib::Bytes::from_owned(offer(mode == "bundled", n).into_bytes());
        signaller.emit_by_name::<()>("post", &[&None::<String>, &body, &promise]);
        let reply = match promise.wait() {
            gst::PromiseResult::Replied => promise.get_reply().unwrap().to_owned(),
            other => panic!("session {n}: no answer: {other:?}"),
        };
        let status = reply.get::<u32>("status").unwrap_or(0);
        previous = reply
            .get::<gst::Structure>("headers")
            .ok()
            .and_then(|headers| headers.get::<String>("location").ok())
            .and_then(|location| location.rsplit('/').next().map(String::from));
        println!("session {n}: answer status {status}");

        if mode == "renotify" {
            let bin = webrtcbin.lock().unwrap().clone().expect("webrtcbin");
            println!("session {n}: ice-gathering-state notified again");
            bin.notify("ice-gathering-state");
        }
        std::thread::sleep(Duration::from_millis(300));
    }

    pipeline.set_state(gst::State::Null).unwrap();
    println!("done: no crash after {sessions} sessions");
}
