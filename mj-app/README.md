# brokk-mj-app

The Mjolnir desktop shell binary.

`mj-app` runs the same remote-control server `mj server` serves, then opens it
in a native WebView. It ships as a separate binary beside `mj` so the platform
webview dependency (WebKitGTK on Linux, WebView2 on Windows) never reaches the
`mj` CLI, which has to start on headless machines.

`mj app` locates and launches this binary; it is not usually run directly.
