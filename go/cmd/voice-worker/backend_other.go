// Port of voice-worker/src/backend.rs: platforms other than Linux have no
// PulseAudio input (Rust compiles it only for target_os = "linux").

//go:build !linux

package main

// buildPulseSource is build_audio_source's Linux branch, absent here: the
// CPAL path always decides.
func buildPulseSource(tx audioSender) (source inputSource, rate uint32, ok bool, err error) {
	return nil, 0, false, nil
}
