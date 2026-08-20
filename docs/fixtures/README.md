# Documentation fixtures

`ten-minute-evaluation/` is the disposable repository used by
`docs/src/content/docs/evaluate.mdx`. It deliberately contains two planted
defects its test suite cannot detect — `fahrenheit()` adds 31 instead of 32,
and `test_fahrenheit_conversion` only asserts truthiness. The evaluation
exists to watch Mjolnir's review find them, so do not "fix" the fixture, and
do not document the defects inside the fixture itself: reviewers read the
fixture's own files, and a README that confesses the bugs turns the planted
defects into documented behavior (a reviewer will then correctly report the
commit as self-consistent).
