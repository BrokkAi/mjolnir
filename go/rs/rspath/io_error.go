package rspath

import "github.com/BrokkAi/mjolnir/go/rs/rsio"

// The std::io::Error values of this package are made here, by go/rs/rsio.

// ioErrorInvalidInput is io::const_error!(io::ErrorKind::InvalidInput,
// message).
func ioErrorInvalidInput(message string) error {
	return rsio.ErrorFromStaticMessage(rsio.ErrorKind_InvalidInput, message)
}

// ioErrorFromOs is io::Error::last_os_error() after a system call that
// failed with err.
func ioErrorFromOs(err error) error {
	return rsio.ErrorLastOsError(err)
}
