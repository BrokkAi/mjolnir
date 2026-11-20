// Port of anyhow 1.0.102 tests/common/mod.rs

package anyhow

// bailLiteral is `bail!("oh no!")`.
func bailLiteral() error {
	return Errorf("oh no!")
}

// bailFmt is `bail!("{} {}!", "oh", "no")`.
func bailFmt() error {
	return Errorf("{} {}!", "oh", "no")
}

// bailError is `bail!(io::Error::new(io::ErrorKind::Other, "oh no!"))`.
func bailError() error {
	return New(ioErrorNew("Other", "oh no!"))
}
