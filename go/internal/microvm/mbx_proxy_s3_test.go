//go:build linux

// Port of the tests of mj-microvm/src/mbx_proxy/s3.rs.

package microvm

import (
	"net/url"
	"testing"
	"time"
)

// Cross-check the raw GET with the independently calculated vector in
// mr-boxington's crates/mbx-cache-core/src/sigv4.rs.
func TestMbxProxySigv4MatchesMbx(t *testing.T) {
	role := microvmS3Credentials{
		AccessKeyID:     "AKIDEXAMPLE",
		SecretAccessKey: "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY",
	}
	target, err := url.Parse("https://examplebucket.s3.amazonaws.com/test.txt")
	if err != nil {
		t.Fatal(err)
	}
	request := microvmMbxSignedGet(target, "us-east-1", role, time.Unix(1_440_938_160, 0).UTC())
	expected := "AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20150830/us-east-1/s3/aws4_request, " +
		"SignedHeaders=host;x-amz-content-sha256;x-amz-date, " +
		"Signature=bbfdf4d3c3eab24da182f8f790e0c7d8e2a20658191717a6546076effa9f5a5e"
	if actual := request.Header.Get("Authorization"); actual != expected {
		t.Fatalf("authorization %q", actual)
	}
}
