// Port of mj-controller/src/controller/update.rs (only the startup notice
// the dashboard shows so far).

package controller

// StartupUpdateNotice is what the startup update check leaves for the
// dashboard's notice bar.
type StartupUpdateNotice interface{ isStartupUpdateNotice() }

// StartupUpdateNoticeAvailable says a newer release can be installed.
type StartupUpdateNoticeAvailable struct{ Text string }

// StartupUpdateNoticeFailed says the check or the upgrade failed.
type StartupUpdateNoticeFailed struct{ Text string }

func (StartupUpdateNoticeAvailable) isStartupUpdateNotice() {}
func (StartupUpdateNoticeFailed) isStartupUpdateNotice()    {}
