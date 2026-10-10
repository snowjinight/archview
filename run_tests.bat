@echo off
rem Run all tests with output shown (e2e playback test prints skip/pass info).
rem Optional: set ZIP_VIDEO_TEST_7Z to your 7z.exe to enable the 7z archive test.
cargo test -- --nocapture
