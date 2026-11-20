// Port of chrono 0.4.44 src/format/locales.rs

package chrono

// Mjolnir builds chrono without the `unstable-locales` feature, so this is
// the crate's `unlocalized` module: the POSIX names.

var shortMonths = [12]string{"Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"}

var longMonths = [12]string{
	"January",
	"February",
	"March",
	"April",
	"May",
	"June",
	"July",
	"August",
	"September",
	"October",
	"November",
	"December",
}

var shortWeekdays = [7]string{"Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"}

var longWeekdays = [7]string{"Sunday", "Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday"}

var amPm = [2]string{"AM", "PM"}

const decimalPoint = "."
