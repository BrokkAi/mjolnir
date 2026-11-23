//go:build linux

// Port of mj-microvm/src/mbx_proxy/s3.rs.
//
// Signed, read-only S3 GETs for the mbx proxy, with credentials from the
// host's EC2 instance role.

package microvm

import (
	"context"
	"crypto/hmac"
	"crypto/sha256"
	"encoding/hex"
	"encoding/xml"
	"errors"
	"fmt"
	"io"
	"net"
	"net/http"
	"net/url"
	"os"
	"strings"
	"sync"
	"time"
)

const (
	// microvmMbxRenewalMargin: AWS instance-profile credentials begin
	// rotating five minutes before expiry. Use the same refresh margin as
	// mbx-cache-core's S3 client.
	microvmMbxRenewalMargin = 5 * time.Minute
	// microvmMbxRenewalRetry: IMDS can return the same credentials during
	// the renewal margin. Match mbx-cache-core's renewal backoff instead of
	// fetching on each cache request.
	microvmMbxRenewalRetry = 30 * time.Second
	// microvmMbxS3Inactivity matches mbx's default connect and response
	// inactivity deadlines.
	microvmMbxS3Inactivity = 30 * time.Second
	// microvmMbxMaxErrorBodyBytes matches the S3 diagnostic body limit in
	// mbx-cache-core.
	microvmMbxMaxErrorBodyBytes = 8 * 1024
	microvmMbxEmptySHA256       = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
)

type microvmMbxS3 struct {
	base     *url.URL
	prefix   string
	region   string
	client   *http.Client
	provider *microvmInstanceRoleCredentials
	roleMu   sync.Mutex
	role     microvmMbxRole
}

type microvmMbxRole struct {
	temporary    microvmTemporaryCredentials
	refreshAfter time.Time
	lastError    *string
}

// microvmMbxS3Error is an S3 refusal: its HTTP status and the error code
// its body named, if any. Rust's ReadError::S3; every other failure is
// ReadError::Other, a plain error.
type microvmMbxS3Error struct {
	status int
	code   *string
}

func (e *microvmMbxS3Error) Error() string { return microvmMbxDiagnostic(e) }

// microvmMbxDiagnostic is ReadError::diagnostic: an S3 refusal's code (or
// its status), or the error chain.
func microvmMbxDiagnostic(err error) string {
	if refusal, ok := err.(*microvmMbxS3Error); ok {
		if refusal.code != nil {
			return *refusal.code
		}
		return fmt.Sprintf("S3 HTTP %d", refusal.status)
	}
	return err.Error()
}

// microvmMbxAccessDenied is ReadError::access_denied.
func microvmMbxAccessDenied(err error) bool {
	refusal, ok := err.(*microvmMbxS3Error)
	return ok && refusal.code != nil && *refusal.code == "AccessDenied"
}

// microvmMbxS3Connect connects to the remote with the instance role, or
// returns nil when the host has no role.
func microvmMbxS3Connect(remote MicrovmMbxRemote) (*microvmMbxS3, error) {
	provider, err := microvmInstanceRoleCredentialsFromEnv()
	if err != nil {
		return nil, err
	}
	if provider == nil {
		return nil, nil
	}
	role, err := provider.fetch()
	if err != nil {
		var status *microvmIMDSStatusError
		listing := strings.HasPrefix(err.Error(), microvmIMDSListRolesContext+": ")
		if errors.Is(err, microvmIMDSNoRoleError) || listing && errors.As(err, &status) && status.status == http.StatusNotFound {
			return nil, nil
		}
		return nil, err
	}
	var region string
	if remote.Region != nil {
		region = *remote.Region
	} else if region, err = provider.region(); err != nil {
		return nil, err
	}
	var endpoint *url.URL
	if value := os.Getenv("MBX_REMOTE_S3_ENDPOINT"); value != "" {
		if endpoint, err = microvmParseAbsoluteURL(value); err != nil {
			return nil, err
		}
	}
	return microvmNewMbxS3(remote, region, provider, role, endpoint)
}

func microvmNewMbxS3(remote MicrovmMbxRemote, region string, provider *microvmInstanceRoleCredentials, role microvmTemporaryCredentials, endpoint *url.URL) (*microvmMbxS3, error) {
	valid := region != ""
	for i := 0; i < len(region); i++ {
		c := region[i]
		valid = valid && (c >= 'a' && c <= 'z' || c >= '0' && c <= '9' || c == '-')
	}
	if !valid {
		return nil, fmt.Errorf("invalid S3 region %s", microvmDebugString(region))
	}
	bucket := remote.Bucket
	domain := "amazonaws.com"
	if strings.HasPrefix(region, "cn-") {
		domain = "amazonaws.com.cn"
	}
	var base *url.URL
	var err error
	switch {
	case endpoint != nil:
		credentialed := false
		if user := endpoint.User; user != nil {
			_, hasPassword := user.Password()
			credentialed = user.Username() != "" || hasPassword
		}
		if (endpoint.Scheme != "http" && endpoint.Scheme != "https") || endpoint.Hostname() == "" || credentialed ||
			endpoint.RawQuery != "" || endpoint.ForceQuery || endpoint.Fragment != "" {
			return nil, errors.New("invalid S3 endpoint")
		}
		copied := *endpoint
		copied.RawPath = ""
		copied.Path = strings.TrimRight(endpoint.Path, "/") + "/" + bucket + "/"
		base = &copied
	case strings.Contains(bucket, "."):
		base, err = url.Parse(fmt.Sprintf("https://s3.%s.%s/%s/", region, domain, bucket))
	default:
		base, err = url.Parse(fmt.Sprintf("https://%s.s3.%s.%s/", bucket, region, domain))
	}
	if err != nil {
		return nil, err
	}
	dialer := &net.Dialer{Timeout: microvmMbxS3Inactivity}
	client := &http.Client{
		Transport: &http.Transport{
			Proxy:              nil,
			DisableCompression: true,
			DialContext: func(ctx context.Context, network, address string) (net.Conn, error) {
				conn, err := dialer.DialContext(ctx, network, address)
				if err != nil {
					return nil, err
				}
				return &microvmInactivityConn{Conn: conn, idle: microvmMbxS3Inactivity}, nil
			},
		},
		CheckRedirect: func(*http.Request, []*http.Request) error {
			return http.ErrUseLastResponse
		},
	}
	return &microvmMbxS3{
		base: base,
		// The literal prefix the IAM policy names; percent escapes are
		// refused.
		prefix:   remote.Prefix,
		region:   region,
		client:   client,
		provider: provider,
		role: microvmMbxRole{
			refreshAfter: role.ExpiresAt.Add(-microvmMbxRenewalMargin),
			temporary:    role,
		},
	}, nil
}

// microvmInactivityConn fails a read that waits longer than idle for data,
// as reqwest's read_timeout does.
type microvmInactivityConn struct {
	net.Conn
	idle time.Duration
}

func (c *microvmInactivityConn) Read(buffer []byte) (int, error) {
	if err := c.Conn.SetReadDeadline(time.Now().Add(c.idle)); err != nil {
		return 0, err
	}
	return c.Conn.Read(buffer)
}

func (s *microvmMbxS3) credentials() (microvmS3Credentials, error) {
	s.roleMu.Lock()
	defer s.roleMu.Unlock()
	role := &s.role
	if !role.refreshAfter.After(time.Now()) {
		fresh, err := s.provider.fetch()
		if err == nil {
			role.temporary = fresh
			role.lastError = nil
		} else {
			message := err.Error()
			if role.lastError == nil {
				fmt.Fprintf(os.Stderr, "could not refresh the EC2 instance role credentials: %s\n", message)
			}
			role.lastError = &message
		}
		role.refreshAfter = role.temporary.ExpiresAt.Add(-microvmMbxRenewalMargin)
		if retry := time.Now().Add(microvmMbxRenewalRetry); retry.After(role.refreshAfter) {
			role.refreshAfter = retry
		}
	}
	if !role.temporary.ExpiresAt.After(time.Now()) {
		suffix := ""
		if role.lastError != nil {
			suffix = ": " + *role.lastError
		}
		return microvmS3Credentials{}, fmt.Errorf("EC2 instance role credentials have expired%s", suffix)
	}
	return role.temporary.Credentials, nil
}

// get reads an object; a nil response means S3 does not hold it. The
// caller closes the response body.
func (s *microvmMbxS3) get(namespace string, route string) (*http.Response, error) {
	key := namespace + route
	if s.prefix != "" {
		key = s.prefix + "/" + namespace + route
	}
	target, err := url.Parse(s.base.String() + microvmMbxEncodeKey(key))
	if err != nil {
		return nil, fmt.Errorf("construct S3 object URL: %w", err)
	}
	credentials, err := s.credentials()
	if err != nil {
		return nil, err
	}
	response, err := s.client.Do(microvmMbxSignedGet(target, s.region, credentials, time.Now().UTC()))
	if err != nil {
		return nil, fmt.Errorf("read S3: %w", err)
	}
	if response.StatusCode == http.StatusOK {
		return response, nil
	}
	var code *string
	if body, err := microvmMbxReadBody(response, microvmMbxMaxErrorBodyBytes); err == nil {
		var document struct {
			Code *string `xml:"Code"`
		}
		if xml.Unmarshal(body, &document) == nil {
			code = document.Code
		}
	}
	if code != nil && *code == "NoSuchKey" || response.StatusCode == http.StatusNotFound && code == nil {
		return nil, nil
	}
	return nil, &microvmMbxS3Error{status: response.StatusCode, code: code}
}

// microvmMbxReadBody reads a response body of at most maxBytes, and closes
// it.
func microvmMbxReadBody(response *http.Response, maxBytes int) ([]byte, error) {
	defer response.Body.Close()
	var body []byte
	buffer := make([]byte, 32*1024)
	for {
		count, err := response.Body.Read(buffer)
		if count > maxBytes-len(body) {
			return nil, fmt.Errorf("S3 response exceeds %d bytes", maxBytes)
		}
		body = append(body, buffer[:count]...)
		if err == io.EOF {
			return body, nil
		}
		if err != nil {
			return nil, err
		}
	}
}

func microvmMbxEncodeKey(key string) string {
	var encoded strings.Builder
	encoded.Grow(len(key))
	for i := 0; i < len(key); i++ {
		b := key[i]
		if microvmIsASCIIAlphanumeric(b) || strings.IndexByte("-._~/", b) >= 0 {
			encoded.WriteByte(b)
		} else {
			fmt.Fprintf(&encoded, "%%%02X", b)
		}
	}
	return encoded.String()
}

// microvmMbxSignedGet is a GET of target signed with AWS Signature
// Version 4 for S3.
func microvmMbxSignedGet(target *url.URL, region string, role microvmS3Credentials, now time.Time) *http.Request {
	date := now.Format("20060102")
	timestamp := now.Format("20060102T150405Z")
	// The host as the request carries it: a default port is not written.
	host := target.Host
	if port := target.Port(); target.Scheme == "http" && port == "80" || target.Scheme == "https" && port == "443" {
		host = strings.TrimSuffix(host, ":"+port)
	}
	headers := fmt.Sprintf("host:%s\nx-amz-content-sha256:%s\nx-amz-date:%s\n", host, microvmMbxEmptySHA256, timestamp)
	names := "host;x-amz-content-sha256;x-amz-date"
	if role.SessionToken != nil {
		headers += fmt.Sprintf("x-amz-security-token:%s\n", *role.SessionToken)
		names = "host;x-amz-content-sha256;x-amz-date;x-amz-security-token"
	}
	canonical := fmt.Sprintf("GET\n%s\n\n%s\n%s\n%s", target.EscapedPath(), headers, names, microvmMbxEmptySHA256)
	scope := fmt.Sprintf("%s/%s/s3/aws4_request", date, region)
	canonicalHash := sha256.Sum256([]byte(canonical))
	toSign := fmt.Sprintf("AWS4-HMAC-SHA256\n%s\n%s\n%s", timestamp, scope, hex.EncodeToString(canonicalHash[:]))
	key := microvmMbxHMAC([]byte("AWS4"+role.SecretAccessKey), date)
	for _, input := range []string{region, "s3", "aws4_request", toSign} {
		key = microvmMbxHMAC(key, input)
	}
	authorization := fmt.Sprintf("AWS4-HMAC-SHA256 Credential=%s/%s, SignedHeaders=%s, Signature=%s",
		role.AccessKeyID, scope, names, hex.EncodeToString(key))
	request, err := http.NewRequest(http.MethodGet, target.String(), nil)
	if err != nil {
		// target is a parsed absolute URL, which always forms a request.
		panic(fmt.Sprintf("S3 request for %s: %v", target, err))
	}
	request.URL, request.Host = target, host
	request.Header.Set("authorization", authorization)
	request.Header.Set("x-amz-content-sha256", microvmMbxEmptySHA256)
	request.Header.Set("x-amz-date", timestamp)
	if role.SessionToken != nil {
		request.Header.Set("x-amz-security-token", *role.SessionToken)
	}
	return request
}

func microvmMbxHMAC(key []byte, data string) []byte {
	mac := hmac.New(sha256.New, key)
	mac.Write([]byte(data))
	return mac.Sum(nil)
}
