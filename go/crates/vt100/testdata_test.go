package vt100

import (
	"archive/tar"
	"bufio"
	"compress/gzip"
	"io"
	"os"
	"path/filepath"
	"strconv"
	"sync"
)

// The readers of the crate's test data, which testdata/vt100-rust/ holds in
// the forms its ORIGIN file describes. They are used by the ports of the
// crate's tests (package vt100_test, it_*_test.go) and by the comparison
// with Rust (oracle_gen_test.go), so they are exported to the tests here.

func testdataOpen(name string) (*gzip.Reader, func(), error) {
	file, err := os.Open(filepath.Join("testdata", "vt100-rust", name))
	if err != nil {
		return nil, nil, err
	}
	unpacked, err := gzip.NewReader(file)
	if err != nil {
		file.Close()
		return nil, nil, err
	}
	return unpacked, func() { file.Close() }, nil
}

// TestdataFixtures is the contents of tests/data/fixtures/, keyed by path
// ("tests/data/fixtures/<name>/<i>.typescript" and ".json").
var TestdataFixtures = sync.OnceValues(func() (map[string][]byte, error) {
	unpacked, closeFile, err := testdataOpen("fixtures.tar.gz")
	if err != nil {
		return nil, err
	}
	defer closeFile()
	files := map[string][]byte{}
	archive := tar.NewReader(unpacked)
	for {
		header, err := archive.Next()
		if err == io.EOF {
			return files, nil
		}
		if err != nil {
			return nil, err
		}
		if header.Typeflag != tar.TypeReg {
			continue
		}
		data, err := io.ReadAll(archive)
		if err != nil {
			return nil, err
		}
		files["tests/data/"+header.Name] = data
	}
})

// TestdataCrawlFrames is tests/data/crawl/crawl1 to crawl7625: frame i is
// element i-1.
var TestdataCrawlFrames = sync.OnceValues(func() ([][]byte, error) {
	unpacked, closeFile, err := testdataOpen("crawl.frames.gz")
	if err != nil {
		return nil, err
	}
	defer closeFile()
	reader := bufio.NewReader(unpacked)
	frames := [][]byte{}
	for {
		line, err := reader.ReadString('\n')
		if err == io.EOF && line == "" {
			return frames, nil
		}
		if err != nil {
			return nil, err
		}
		length, err := strconv.Atoi(line[:len(line)-1])
		if err != nil {
			return nil, err
		}
		frame := make([]byte, length)
		if _, err := io.ReadFull(reader, frame); err != nil {
			return nil, err
		}
		frames = append(frames, frame)
	}
})

// TestdataWeechat is tests/data/weechat.typescript.
var TestdataWeechat = sync.OnceValues(func() ([]byte, error) {
	unpacked, closeFile, err := testdataOpen("weechat.typescript.gz")
	if err != nil {
		return nil, err
	}
	defer closeFile()
	return io.ReadAll(unpacked)
})
