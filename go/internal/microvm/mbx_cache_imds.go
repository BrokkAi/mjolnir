//go:build linux

// Credentials from an EC2 instance role, through IMDSv2, for the mbx
// proxy's S3 reads (mbx_proxy_s3.go). Rust takes this provider from the
// mbx-cache-core crate (0.21, `InstanceRoleCredentials`); the proxy's
// no-role detection and refresh depend on its requests, retries and error
// text, so they are reproduced here on net/http.
//
// Three small requests against the instance metadata service: a session
// token, the name of the attached role, and that role's temporary
// credentials. The AWS SDKs and CLI do the same as the last step of their
// credential chain.

package microvm

import (
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"net"
	"net/http"
	"net/url"
	"os"
	"strconv"
	"strings"
	"time"
)

const (
	// microvmIMDSDefaultEndpoint is where the metadata service lives on
	// every EC2 instance.
	microvmIMDSDefaultEndpoint = "http://169.254.169.254"
	// microvmIMDSTokenTTLSeconds is the lifetime requested for the session
	// token. It is used within a moment and never kept, so this only has to
	// be a value the service accepts.
	microvmIMDSTokenTTLSeconds = "21600"
	// microvmIMDSRequestTimeout is how long one request may take, as
	// mbx-cache-core sets it. The address is link-local, so anything slower
	// means no metadata service is listening.
	microvmIMDSRequestTimeout = time.Second
	// microvmIMDSAttempts is the attempts per request. The service
	// throttles per instance, and AWS asks clients to retry with
	// exponential backoff.
	microvmIMDSAttempts = 3
	// microvmIMDSRetryBackoff is the wait before the second attempt; each
	// later wait doubles.
	microvmIMDSRetryBackoff = 200 * time.Millisecond
	// microvmIMDSHopLimitHint is added when the token request times out
	// after connecting, which is what a hop limit of 1 looks like from
	// inside a container.
	microvmIMDSHopLimitHint = "the metadata service accepted the connection but never answered. " +
		"From a container, the instance's metadata hop limit may be 1. Raising " +
		"HttpPutResponseHopLimit to 2 with `aws ec2 modify-instance-metadata-options` fixes " +
		"that for every container on the instance, so first consider whether they should all " +
		"be able to use its role"
	microvmIMDSTokenTTLHeader = "x-aws-ec2-metadata-token-ttl-seconds"
	microvmIMDSTokenHeader    = "x-aws-ec2-metadata-token"
)

// microvmS3Credentials are the credentials a request is signed with.
type microvmS3Credentials struct {
	AccessKeyID     string
	SecretAccessKey string
	SessionToken    *string
}

// microvmTemporaryCredentials are temporary credentials and the instant
// they stop working.
type microvmTemporaryCredentials struct {
	Credentials microvmS3Credentials
	ExpiresAt   time.Time
}

// microvmInstanceRoleCredentials is a source of credentials for the EC2
// instance role attached to this machine.
type microvmInstanceRoleCredentials struct {
	client   *http.Client
	endpoint *url.URL
	backoff  time.Duration
}

// microvmIMDSStatusError is an answer outside 2xx.
type microvmIMDSStatusError struct{ status int }

func (e *microvmIMDSStatusError) Error() string {
	return "the metadata service answered " + microvmHTTPStatus(e.status)
}

// microvmIMDSNoRoleError is a lookup that found no role attached.
var microvmIMDSNoRoleError = errors.New("the instance has no IAM role attached")

// microvmIMDSListRolesContext names a failure to list the roles.
const microvmIMDSListRolesContext = "could not list the instance's IAM roles"

// microvmHTTPStatus is a status as the Rust http crate displays it:
// `404 Not Found`.
func microvmHTTPStatus(status int) string {
	reason := http.StatusText(status)
	if reason == "" {
		reason = "<unknown status code>"
	}
	return strconv.Itoa(status) + " " + reason
}

// microvmNewInstanceRoleCredentials is a provider for the metadata service
// at endpoint.
func microvmNewInstanceRoleCredentials(endpoint *url.URL) *microvmInstanceRoleCredentials {
	return &microvmInstanceRoleCredentials{
		client: &http.Client{
			Transport: &http.Transport{
				// A link-local address is never behind a proxy, and sending
				// role credentials through one would hand them to it.
				Proxy:       nil,
				DialContext: (&net.Dialer{Timeout: microvmIMDSRequestTimeout}).DialContext,
			},
			Timeout: 2 * microvmIMDSRequestTimeout,
			CheckRedirect: func(*http.Request, []*http.Request) error {
				return http.ErrUseLastResponse
			},
		},
		endpoint: endpoint,
		backoff:  microvmIMDSRetryBackoff,
	}
}

// microvmInstanceRoleCredentialsFromEnv is a provider configured the way
// the AWS SDKs read it, or nil when AWS_EC2_METADATA_DISABLED turns
// instance role lookup off. AWS_EC2_METADATA_SERVICE_ENDPOINT overrides the
// address, for a proxy in front of the service or a test double.
func microvmInstanceRoleCredentialsFromEnv() (*microvmInstanceRoleCredentials, error) {
	if disabled, ok := os.LookupEnv("AWS_EC2_METADATA_DISABLED"); ok && strings.EqualFold(strings.TrimSpace(disabled), "true") {
		return nil, nil
	}
	endpoint := microvmIMDSDefaultEndpoint
	if value, ok := os.LookupEnv("AWS_EC2_METADATA_SERVICE_ENDPOINT"); ok && strings.TrimSpace(value) != "" {
		endpoint = strings.TrimSpace(value)
	}
	parsed, err := microvmParseAbsoluteURL(endpoint)
	if err != nil {
		return nil, fmt.Errorf("invalid AWS_EC2_METADATA_SERVICE_ENDPOINT: %w", err)
	}
	return microvmNewInstanceRoleCredentials(parsed), nil
}

// microvmParseAbsoluteURL parses an absolute URL, which the Rust url
// crate requires: a scheme and, for http(s), a host.
func microvmParseAbsoluteURL(text string) (*url.URL, error) {
	parsed, err := url.Parse(text)
	if err != nil {
		return nil, err
	}
	if parsed.Scheme == "" {
		return nil, errors.New("relative URL without a base")
	}
	if (parsed.Scheme == "http" || parsed.Scheme == "https") && parsed.Host == "" {
		return nil, errors.New("empty host")
	}
	if parsed.Path == "" && parsed.Opaque == "" && parsed.Host != "" {
		parsed.Path = "/"
	}
	return parsed, nil
}

// fetch asks the metadata service for the role's current credentials.
func (p *microvmInstanceRoleCredentials) fetch() (microvmTemporaryCredentials, error) {
	token, err := p.token()
	if err != nil {
		return microvmTemporaryCredentials{}, fmt.Errorf("could not get an IMDSv2 session token: %w", err)
	}
	roles, err := p.get(token, "latest/meta-data/iam/security-credentials/")
	if err != nil {
		return microvmTemporaryCredentials{}, fmt.Errorf("%s: %w", microvmIMDSListRolesContext, err)
	}
	role := ""
	for _, line := range microvmLines(roles) {
		if line = strings.TrimSpace(line); line != "" {
			role = line
			break
		}
	}
	if role == "" {
		return microvmTemporaryCredentials{}, microvmIMDSNoRoleError
	}
	document, err := p.get(token, "latest/meta-data/iam/security-credentials/"+role)
	if err != nil {
		return microvmTemporaryCredentials{}, fmt.Errorf("could not read credentials for role %s: %w", role, err)
	}
	return microvmParseRoleDocument(document)
}

// region is the region the instance runs in, for a machine that names none
// itself.
func (p *microvmInstanceRoleCredentials) region() (string, error) {
	token, err := p.token()
	if err != nil {
		return "", fmt.Errorf("could not get an IMDSv2 session token: %w", err)
	}
	region, err := p.get(token, "latest/meta-data/placement/region")
	if err != nil {
		return "", fmt.Errorf("could not read the instance's region: %w", err)
	}
	region = strings.TrimSpace(region)
	if region == "" {
		return "", errors.New("the metadata service returned an empty region")
	}
	return region, nil
}

func (p *microvmInstanceRoleCredentials) token() (string, error) {
	target, err := p.endpoint.Parse("latest/api/token")
	if err != nil {
		return "", err
	}
	token, err := p.send(http.MethodPut, target, map[string]string{microvmIMDSTokenTTLHeader: microvmIMDSTokenTTLSeconds})
	if err != nil {
		if microvmIsTimeout(err) && !microvmIsConnectError(err) {
			return "", fmt.Errorf("%s: %w", microvmIMDSHopLimitHint, err)
		}
		return "", err
	}
	return strings.TrimSpace(token), nil
}

func (p *microvmInstanceRoleCredentials) get(token string, path string) (string, error) {
	target, err := p.endpoint.Parse(path)
	if err != nil {
		return "", err
	}
	return p.send(http.MethodGet, target, map[string]string{microvmIMDSTokenHeader: token})
}

// send sends a request and returns the body of a successful answer.
// Throttling, server errors, and a request that connected but got no answer
// are tried again. A refusal to connect is not: nothing is listening.
func (p *microvmInstanceRoleCredentials) send(method string, target *url.URL, headers map[string]string) (string, error) {
	for attempt := 1; ; attempt++ {
		request, err := http.NewRequest(method, target.String(), nil)
		if err != nil {
			return "", err
		}
		for name, value := range headers {
			request.Header.Set(name, value)
		}
		var failure error
		transient := false
		response, err := p.client.Do(request)
		switch {
		case err != nil:
			failure, transient = err, !microvmIsConnectError(err)
		case response.StatusCode >= 200 && response.StatusCode < 300:
			body, readErr := io.ReadAll(response.Body)
			response.Body.Close()
			if readErr == nil {
				return microvmLossy(body), nil
			}
			failure, transient = readErr, true
		default:
			response.Body.Close()
			failure = &microvmIMDSStatusError{status: response.StatusCode}
			transient = response.StatusCode == http.StatusTooManyRequests || response.StatusCode >= 500
		}
		if !transient || attempt == microvmIMDSAttempts {
			return "", failure
		}
		time.Sleep(p.backoff * time.Duration(1<<(attempt-1)))
	}
}

// microvmIsConnectError reports whether err is a failure to connect.
func microvmIsConnectError(err error) bool {
	var operation *net.OpError
	return errors.As(err, &operation) && operation.Op == "dial"
}

func microvmIsTimeout(err error) bool {
	var timeout interface{ Timeout() bool }
	return errors.As(err, &timeout) && timeout.Timeout()
}

// microvmParseRoleDocument reads the JSON document under
// `security-credentials/<role>`. Every member is optional because a failing
// lookup returns a document with a `Code` and none of the credentials. The
// body is never included in an error: it carries the secret.
func microvmParseRoleDocument(document string) (microvmTemporaryCredentials, error) {
	var members map[string]json.RawMessage
	unreadable := errors.New("the metadata service returned a credentials document mbx cannot read")
	if err := json.Unmarshal([]byte(document), &members); err != nil || members == nil {
		return microvmTemporaryCredentials{}, unreadable
	}
	field := func(name string) (*string, error) {
		raw, ok := members[name]
		if !ok {
			return nil, nil
		}
		var value *string
		if err := json.Unmarshal(raw, &value); err != nil {
			return nil, unreadable
		}
		return value, nil
	}
	var values [5]*string
	for i, name := range []string{"Code", "AccessKeyId", "SecretAccessKey", "Token", "Expiration"} {
		value, err := field(name)
		if err != nil {
			return microvmTemporaryCredentials{}, err
		}
		values[i] = value
	}
	code, accessKeyID, secretAccessKey, token, expiration := values[0], values[1], values[2], values[3], values[4]
	if code != nil && *code != "Success" {
		return microvmTemporaryCredentials{}, fmt.Errorf("the metadata service reported %s for the instance role", *code)
	}
	if accessKeyID == nil || secretAccessKey == nil || expiration == nil {
		return microvmTemporaryCredentials{}, errors.New("the instance role's credentials document is missing fields")
	}
	expiresAt, err := microvmParseIMDSTimestamp(*expiration)
	if err != nil {
		return microvmTemporaryCredentials{}, err
	}
	if token != nil && *token == "" {
		token = nil
	}
	return microvmTemporaryCredentials{
		Credentials: microvmS3Credentials{AccessKeyID: *accessKeyID, SecretAccessKey: *secretAccessKey, SessionToken: token},
		ExpiresAt:   expiresAt,
	}, nil
}

// microvmParseIMDSTimestamp parses the `2026-09-29T18:45:12Z` form the
// metadata service uses. A fractional second is accepted and dropped; a
// numeric UTC offset is not, since the service always answers in UTC.
func microvmParseIMDSTimestamp(text string) (time.Time, error) {
	invalid := errors.New("the instance role's expiration is not a UTC timestamp")
	text, ok := strings.CutSuffix(strings.TrimSpace(text), "Z")
	if !ok {
		return time.Time{}, invalid
	}
	date, clock, ok := strings.Cut(text, "T")
	if !ok {
		return time.Time{}, invalid
	}
	clock, _, _ = strings.Cut(clock, ".")
	numbers := func(text string) ([]int64, bool) {
		var values []int64
		for _, part := range strings.Split(text, "-") {
			value, err := microvmParseInt64(part)
			if err != nil {
				return nil, false
			}
			values = append(values, value)
		}
		return values, true
	}
	dateParts, ok := numbers(date)
	if !ok || len(dateParts) != 3 {
		return time.Time{}, invalid
	}
	var clockParts []int64
	for _, part := range strings.Split(clock, ":") {
		value, err := microvmParseInt64(part)
		if err != nil {
			return time.Time{}, invalid
		}
		clockParts = append(clockParts, value)
	}
	if len(clockParts) != 3 {
		return time.Time{}, invalid
	}
	year, month, day := dateParts[0], dateParts[1], dateParts[2]
	hour, minute, second := clockParts[0], clockParts[1], clockParts[2]
	if month < 1 || month > 12 || day < 1 || day > 31 || hour < 0 || hour >= 24 || minute < 0 || minute >= 60 || second < 0 || second > 60 {
		return time.Time{}, invalid
	}
	seconds := microvmDaysFromCivil(year, month, day)*86_400 + hour*3_600 + minute*60 + second
	if seconds < 0 {
		return time.Time{}, invalid
	}
	return time.Unix(seconds, 0), nil
}

// microvmDaysFromCivil is the days since the Unix epoch of a proleptic
// Gregorian date.
func microvmDaysFromCivil(year, month, day int64) int64 {
	if month <= 2 {
		year--
	}
	era := year
	if era < 0 {
		era -= 399
	}
	era /= 400
	yearOfEra := year - era*400
	shiftedMonth := month + 9
	if month > 2 {
		shiftedMonth = month - 3
	}
	dayOfYear := (153*shiftedMonth+2)/5 + day - 1
	dayOfEra := yearOfEra*365 + yearOfEra/4 - yearOfEra/100 + dayOfYear
	return era*146_097 + dayOfEra - 719_468
}
