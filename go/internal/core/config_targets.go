// Port of mj-core/src/config/targets.rs.
//
// Projects, containers and the target templates a session runs on.

package core

import (
	"encoding/json"
	"errors"
	"fmt"
	"log/slog"
	"math"
	"path/filepath"
	"strconv"
	"strings"
	"unicode"
)

// ContainerMemoryLimit is ParseContainerMemory as the signed byte count an
// engine takes.
func ContainerMemoryLimit(text string) (int64, error) {
	bytes, err := ParseContainerMemory(text)
	if err != nil {
		return 0, err
	}
	if bytes > math.MaxInt64 {
		return 0, errors.New("out of range integral type conversion attempted")
	}
	return int64(bytes), nil
}

// ParseContainerMemory reads an engine memory limit in bytes, using binary
// units for Podman suffixes.
func ParseContainerMemory(text string) (uint64, error) {
	text = strings.TrimSpace(text)
	value := strings.TrimRightFunc(text, func(r rune) bool {
		return r < 0x80 && (r >= 'a' && r <= 'z' || r >= 'A' && r <= 'Z')
	})
	var shift uint
	var unit string
	switch configASCIILower(text[len(value):]) {
	case "", "b":
		shift, unit = 0, "B"
	case "k", "kb", "ki", "kib":
		shift, unit = 10, "KiB"
	case "m", "mb", "mi", "mib":
		shift, unit = 20, "MiB"
	case "g", "gb", "gi", "gib":
		shift, unit = 30, "GiB"
	case "t", "tb", "ti", "tib":
		shift, unit = 40, "TiB"
	default:
		return 0, fmt.Errorf("invalid container memory limit %s", debugString(text))
	}
	if amount, ok := parseRustUnsigned(strings.TrimSpace(value)); ok {
		if amount != 0 && amount > math.MaxUint64>>shift {
			return 0, fmt.Errorf("container memory limit %s is too large", debugString(text))
		}
		return amount << shift, nil
	}
	// Keep support for fractional quantities in container upgrade settings.
	bytes, err := parseByteSizeValue(value + unit)
	if err != nil {
		return 0, err
	}
	if bytes == math.MaxUint64 {
		return 0, fmt.Errorf("container memory limit %s is too large", debugString(text))
	}
	return bytes, nil
}

type ProjectRepository struct {
	// ID is the stable name within the bundle, used by PrimaryRepo.
	ID string `json:"id" toml:"id"`
	// Github is a network Git URL (or GitHub `owner/repository` shorthand).
	Github *string `json:"github,omitempty" toml:"github,omitempty"`
	// MbxNamespace is the S3 build cache namespace. Defaults to the GitHub
	// owner/repository.
	MbxNamespace *string `json:"mbx_namespace,omitempty" toml:"mbx_namespace,omitempty"`
	// Local is a controller-side repository whose default network remotes
	// seed isolated sessions.
	Local *string `json:"local,omitempty" toml:"local,omitempty"`
	// Destination is a safe relative path beneath the target's bundle root.
	Destination string  `json:"destination" toml:"destination"`
	GitRef      *string `json:"git_ref,omitempty" toml:"git_ref,omitempty"`
}

// UnmarshalJSON requires `id` and `destination`, as serde does.
func (p *ProjectRepository) UnmarshalJSON(data []byte) error {
	if err := requireFields(data, "id", "destination"); err != nil {
		return err
	}
	type plain ProjectRepository
	var value plain
	if err := json.Unmarshal(data, &value); err != nil {
		return err
	}
	*p = ProjectRepository(value)
	return nil
}

type ProjectBundle struct {
	// PrimaryRepo is the repository id used as the ACP session cwd.
	PrimaryRepo  string              `json:"primary_repo" toml:"primary_repo"`
	Repositories []ProjectRepository `json:"repositories" toml:"repositories"`
}

// UnmarshalJSON requires both fields, as serde does.
func (p *ProjectBundle) UnmarshalJSON(data []byte) error {
	if err := requireFields(data, "primary_repo", "repositories"); err != nil {
		return err
	}
	type plain ProjectBundle
	var value plain
	if err := json.Unmarshal(data, &value); err != nil {
		return err
	}
	*p = ProjectBundle(value)
	return nil
}

func (p ProjectBundle) validate(bundleID string) error {
	if err := ValidateID("bundle", bundleID); err != nil {
		return err
	}
	if len(p.Repositories) == 0 {
		return fmt.Errorf("bundle %s must contain at least one repository", debugString(bundleID))
	}

	ids := map[string]bool{}
	var destinations []string
	for _, repository := range p.Repositories {
		if err := ValidateID("repository", repository.ID); err != nil {
			return fmt.Errorf("bundle %s: %w", debugString(bundleID), err)
		}
		if ids[repository.ID] {
			return fmt.Errorf("bundle %s contains duplicate repository id %s", debugString(bundleID), debugString(repository.ID))
		}
		ids[repository.ID] = true
		if (repository.Github != nil) == (repository.Local != nil) {
			return fmt.Errorf("bundle %s repository %s must declare exactly one of `github` or `local`",
				debugString(bundleID), debugString(repository.ID))
		}
		if repository.Github != nil {
			if err := validateConfiguredProjectSource(*repository.Github); err != nil {
				return fmt.Errorf("invalid Git source for bundle %s repository %s: %w",
					debugString(bundleID), debugString(repository.ID), err)
			}
		}
		if repository.Local != nil && !filepath.IsAbs(*repository.Local) {
			return fmt.Errorf("bundle %s repository %s local path must be absolute",
				debugString(bundleID), debugString(repository.ID))
		}
		if repository.GitRef != nil {
			return fmt.Errorf("bundle %s repository %s: git_ref is no longer supported; remove it to start from the remote's default branch",
				debugString(bundleID), debugString(repository.ID))
		}
		if err := ValidateRelativeDestination(repository.Destination); err != nil {
			return fmt.Errorf("bundle %s repository %s destination: %w", debugString(bundleID), debugString(repository.ID), err)
		}
		for _, existing := range destinations {
			if pathStartsWith(repository.Destination, existing) || pathStartsWith(existing, repository.Destination) {
				return fmt.Errorf("bundle %s contains overlapping destinations %s and %s",
					debugString(bundleID), existing, repository.Destination)
			}
		}
		destinations = append(destinations, repository.Destination)
	}
	if !ids[p.PrimaryRepo] {
		return fmt.Errorf("bundle %s primary repository %s does not exist", debugString(bundleID), debugString(p.PrimaryRepo))
	}
	if _, err := p.MbxNamespaces(); err != nil {
		return fmt.Errorf("bundle %s: %w", debugString(bundleID), err)
	}
	return nil
}

func (p ProjectBundle) Primary() *ProjectRepository {
	for i := range p.Repositories {
		if p.Repositories[i].ID == p.PrimaryRepo {
			return &p.Repositories[i]
		}
	}
	return nil
}

// MbxNamespaces is the session's default namespace followed by its other
// allowed namespaces.
func (p ProjectBundle) MbxNamespaces() ([]string, error) {
	var repositories []ProjectRepository
	if primary := p.Primary(); primary != nil {
		repositories = append(repositories, *primary)
	}
	for _, repository := range p.Repositories {
		if repository.ID != p.PrimaryRepo {
			repositories = append(repositories, repository)
		}
	}
	seen := map[string]bool{}
	namespaces := []string{}
	for _, repository := range repositories {
		var namespace string
		switch {
		case repository.MbxNamespace != nil:
			namespace = *repository.MbxNamespace
		case repository.Github != nil:
			owner, name, ok := GithubOwnerRepo(*repository.Github)
			if !ok {
				continue
			}
			namespace = owner + "/" + name
		default:
			continue
		}
		for _, segment := range strings.Split(namespace, "/") {
			valid := segment != "" && segment != "." && segment != ".."
			for i := 0; valid && i < len(segment); i++ {
				b := segment[i]
				valid = b < 0x80 && (b >= 'a' && b <= 'z' || b >= 'A' && b <= 'Z' || b >= '0' && b <= '9' ||
					b == '.' || b == '_' || b == '-')
			}
			if !valid {
				return nil, fmt.Errorf("repository %s has invalid mbx_namespace %s",
					debugString(repository.ID), debugString(namespace))
			}
		}
		if !seen[namespace] {
			seen[namespace] = true
			namespaces = append(namespaces, namespace)
		}
	}
	return namespaces, nil
}

// stripHTTPUserinfo removes HTTP credentials from the repository's source,
// warning that it did.
func (p *ProjectRepository) stripHTTPUserinfo() {
	if p.Github == nil {
		return
	}
	if clean, owned, err := ProjectSource(*p.Github, ShorthandAllowed); err == nil && owned {
		p.Github = &clean
		slog.Warn(HTTPUserinfoRemoved)
	}
}

func (p ProjectRepository) SourceLabel() string {
	switch {
	case p.Github != nil:
		return *p.Github
	case p.Local != nil:
		return *p.Local
	}
	return "invalid repository source"
}

func (p ProjectRepository) IsLocal() bool { return p.Local != nil }

// TargetBuildCache is the machine policy for the mbx build cache. Every field
// is optional: an unset field keeps the resolved default for that host.
type TargetBuildCache struct {
	Enabled *bool `json:"enabled,omitempty" toml:"enabled,omitempty"`
	// Directory is the cache directory on the target's own host, not on this
	// machine.
	Directory *string `json:"directory,omitempty" toml:"directory,omitempty"`
	// ToolsDirectory holds native Go, Gradle, Turbo, Nx and Bazel caches on
	// this machine. mbx continues to own its separate Rust/C/C++ storage and
	// configuration.
	ToolsDirectory *string `json:"tools_directory,omitempty" toml:"tools_directory,omitempty"`
	// MaxTotalSize is the shared storage budget, an mbx size string such as
	// `100GiB`.
	MaxTotalSize *string `json:"max_total_size,omitempty" toml:"max_total_size,omitempty"`
	// Remote is the S3 cache read by the Nitro host on behalf of its guests.
	Remote *string `json:"remote,omitempty" toml:"remote,omitempty"`
	// RemoteRegion is the S3 bucket region. Defaults to the Nitro host's
	// region.
	RemoteRegion *string `json:"remote_region,omitempty" toml:"remote_region,omitempty"`
	// Scheduler is machine-wide compiler admission settings shared by all
	// builds on this host.
	Scheduler BuildCacheScheduler `json:"scheduler" toml:"scheduler"`
}

func (t TargetBuildCache) MarshalJSON() ([]byte, error) {
	var object serdeObject
	object.fieldUnless(t.Enabled == nil, "enabled", t.Enabled)
	object.fieldUnless(t.Directory == nil, "directory", t.Directory)
	object.fieldUnless(t.ToolsDirectory == nil, "tools_directory", t.ToolsDirectory)
	object.fieldUnless(t.MaxTotalSize == nil, "max_total_size", t.MaxTotalSize)
	object.fieldUnless(t.Remote == nil, "remote", t.Remote)
	object.fieldUnless(t.RemoteRegion == nil, "remote_region", t.RemoteRegion)
	object.fieldUnless(t.Scheduler.IsDefault(), "scheduler", t.Scheduler)
	return object.finish()
}

// UnmarshalJSON reads through buildCacheSettings (serde from), which consumes
// obsolete budgets.
func (t *TargetBuildCache) UnmarshalJSON(data []byte) error {
	var settings buildCacheSettings
	if err := json.Unmarshal(data, &settings); err != nil {
		return err
	}
	*t = targetBuildCacheFromSettings(settings)
	return nil
}

// BuildCacheScheduler is optional machine-wide mbx compiler scheduling
// overrides.
//
// When a field is unset, mbx chooses its own default for the host or
// container. Memory is an admission budget used to weight concurrent
// compilations, not an operating-system memory limit.
type BuildCacheScheduler struct {
	// CPUs is the number of compiler permits shared across all mbx builds on
	// the machine.
	CPUs *uint64 `json:"cpus,omitempty" toml:"cpus,omitempty"`
	// Memory is an mbx size string such as `8GiB`, or `none` to disable
	// memory weighting.
	Memory *string `json:"memory,omitempty" toml:"memory,omitempty"`
}

func (b BuildCacheScheduler) IsDefault() bool { return b.CPUs == nil && b.Memory == nil }

// buildCacheSettings consumes obsolete budgets without interpreting or
// migrating their values. They are read whole rather than as IgnoredAny,
// which the file loader counts as a key no setting reads.
type buildCacheSettings struct {
	Enabled             *bool               `json:"enabled"`
	Directory           *string             `json:"directory"`
	ToolsDirectory      *string             `json:"tools_directory"`
	MaxTotalSize        *string             `json:"max_total_size"`
	Remote              *string             `json:"remote"`
	RemoteRegion        *string             `json:"remote_region"`
	Scheduler           BuildCacheScheduler `json:"scheduler"`
	LegacyMaxSize       json.RawMessage     `json:"max_size"`
	LegacyTargetMaxSize json.RawMessage     `json:"target_max_size"`
}

func targetBuildCacheFromSettings(settings buildCacheSettings) TargetBuildCache {
	return TargetBuildCache{
		Enabled:        settings.Enabled,
		Directory:      settings.Directory,
		ToolsDirectory: settings.ToolsDirectory,
		MaxTotalSize:   settings.MaxTotalSize,
		Remote:         settings.Remote,
		RemoteRegion:   settings.RemoteRegion,
		Scheduler:      settings.Scheduler,
	}
}

func (t TargetBuildCache) IsDefault() bool {
	return t.Enabled == nil && t.Directory == nil && t.ToolsDirectory == nil && t.MaxTotalSize == nil &&
		t.Remote == nil && t.RemoteRegion == nil && t.Scheduler.IsDefault()
}

func (t TargetBuildCache) validate(templateID string) error {
	if t.Remote != nil {
		if _, _, err := MbxS3Location(*t.Remote); err != nil {
			return fmt.Errorf("target template %s build cache: %w", debugString(templateID), err)
		}
	}
	if t.ToolsDirectory != nil && !IsAbsolute(*t.ToolsDirectory) {
		return fmt.Errorf("target template %s build cache tools_directory must be absolute", debugString(templateID))
	}
	if t.Directory != nil && !IsAbsolute(*t.Directory) {
		return fmt.Errorf("target template %s build cache directory must be absolute", debugString(templateID))
	}
	if t.MaxTotalSize != nil {
		if _, ok := ParseBuildCacheSize(*t.MaxTotalSize); !ok {
			return fmt.Errorf("target template %s build cache size %s is not a size such as 100GiB",
				debugString(templateID), debugString(*t.MaxTotalSize))
		}
	}
	if cpus := t.Scheduler.CPUs; cpus != nil && (*cpus == 0 || *cpus > math.MaxInt64) {
		return fmt.Errorf("machine %s build cache scheduler cpus must be a positive integer supported by mbx", debugString(templateID))
	}
	if memory := t.Scheduler.Memory; memory != nil && !isBuildCacheSchedulerMemory(*memory) {
		return fmt.Errorf("machine %s build cache scheduler memory must be an mbx size such as 8GiB or \"none\"", debugString(templateID))
	}
	return nil
}

// ValidMbxS3Bucket checks the bucket name's length, alphabet and endpoints
// for an mbx S3 remote.
func ValidMbxS3Bucket(bucket string) bool {
	if len(bucket) < 3 || len(bucket) > 63 {
		return false
	}
	for i := 0; i < len(bucket); i++ {
		b := bucket[i]
		if !(b >= 'a' && b <= 'z' || b >= '0' && b <= '9' || b == '.' || b == '-') {
			return false
		}
	}
	alphanumeric := func(b byte) bool { return b >= 'a' && b <= 'z' || b >= '0' && b <= '9' }
	return alphanumeric(bucket[0]) && alphanumeric(bucket[len(bucket)-1])
}

// MbxS3Location splits an mbx S3 cache location into its bucket and prefix,
// allowing the bucket root as its prefix.
func MbxS3Location(remote string) (string, string, error) {
	location, ok := strings.CutPrefix(remote, "s3://")
	if !ok {
		location = ""
	}
	bucket, prefix, _ := strings.Cut(location, "/")
	if !ValidMbxS3Bucket(bucket) {
		return "", "", fmt.Errorf("build cache remote %s must be s3://<bucket>/<prefix>", debugString(remote))
	}
	prefix = strings.Trim(prefix, "/")
	for _, segment := range strings.Split(prefix, "/") {
		if segment == "." || segment == ".." {
			return "", "", fmt.Errorf("build cache remote %s contains an S3 prefix with a dot segment", debugString(remote))
		}
	}
	// The IAM policy and the host proxy must read the prefix the same way;
	// a percent escape would mean one literal key to IAM and another to S3.
	if strings.Contains(prefix, "%") {
		return "", "", fmt.Errorf("build cache remote %s must spell its S3 prefix without percent escapes", debugString(remote))
	}
	return bucket, prefix, nil
}

func isBuildCacheSchedulerMemory(value string) bool {
	value = strings.TrimSpace(value)
	return configASCIILower(value) == "none" || parseByteSize(value)
}

// parseByteSize is whether the bytesize crate's FromStr accepts value.
func parseByteSize(value string) bool {
	_, err := parseByteSizeValue(value)
	return err == nil
}

// parseByteSizeValue is the bytesize crate's (2.7) FromStr: a whole byte
// count, or a number (digits and dots) with optional whitespace and a
// decimal or binary unit.
func parseByteSizeValue(value string) (uint64, error) {
	if count, ok := parseRustUnsigned(value); ok {
		return count, nil
	}
	end := 0
	for end < len(value) && (value[end] >= '0' && value[end] <= '9' || value[end] == '.') {
		end++
	}
	number := value[:end]
	if !parseRustFloat(number) {
		reason := "invalid float literal"
		if number == "" {
			reason = "cannot parse float from empty string"
		}
		return 0, fmt.Errorf("couldn't parse %s into a ByteSize, %s", debugString(value), reason)
	}
	float, _ := strconv.ParseFloat(number, 64)
	suffix := strings.TrimLeftFunc(value[end:], unicode.IsSpace)
	var factor uint64
	switch configASCIILower(suffix) {
	case "b":
		factor = 1
	case "k", "kb":
		factor = 1_000
	case "m", "mb":
		factor = 1_000_000
	case "g", "gb":
		factor = 1_000_000_000
	case "t", "tb":
		factor = 1_000_000_000_000
	case "p", "pb":
		factor = 1_000_000_000_000_000
	case "e", "eb":
		factor = 1_000_000_000_000_000_000
	case "ki", "kib":
		factor = 1 << 10
	case "mi", "mib":
		factor = 1 << 20
	case "gi", "gib":
		factor = 1 << 30
	case "ti", "tib":
		factor = 1 << 40
	case "pi", "pib":
		factor = 1 << 50
	case "ei", "eib":
		factor = 1 << 60
	default:
		unit := suffix
		if len(unit) > 3 {
			cut := 3
			for cut < len(unit) && !utf8RuneStart(unit[cut]) {
				cut++
			}
			unit = unit[:cut] + "..."
		}
		return 0, fmt.Errorf("couldn't parse %s into a known SI unit, Failed to parse unit \"%s\"", debugString(suffix), unit)
	}
	// Exact integer arithmetic when the number has no fractional part: f64
	// only has a 53-bit mantissa.
	if !strings.Contains(number, ".") {
		if count, ok := parseRustUnsigned(number); ok {
			if count != 0 && count > math.MaxUint64/factor {
				return math.MaxUint64, nil
			}
			return count * factor, nil
		}
	}
	return rustF64ToU64(float * float64(factor)), nil
}

// utf8RuneStart reports whether b begins a UTF-8 sequence.
func utf8RuneStart(b byte) bool { return b&0xC0 != 0x80 }

// rustF64ToU64 is Rust's saturating `f64 as u64`.
func rustF64ToU64(value float64) uint64 {
	switch {
	case value != value || value <= 0:
		return 0
	case value >= 18446744073709551615.0:
		return math.MaxUint64
	}
	return uint64(value)
}

// parseRustUnsigned is Rust's `str::parse::<u64>`: decimal digits with an
// optional leading `+`.
func parseRustUnsigned(text string) (uint64, bool) {
	text = strings.TrimPrefix(text, "+")
	if text == "" || text[0] < '0' || text[0] > '9' {
		return 0, false
	}
	value, err := strconv.ParseUint(text, 10, 64)
	return value, err == nil
}

// parseRustFloat is whether Rust's `str::parse::<f64>` accepts digits and
// dots: at least one digit and at most one dot.
func parseRustFloat(text string) bool {
	if strings.Count(text, ".") > 1 {
		return false
	}
	return strings.ContainsFunc(text, func(r rune) bool { return r >= '0' && r <= '9' })
}

// isDefaultTargetBuildCache reports whether a target carries no build cache
// overrides, so an unchanged target is not rewritten with an empty section.
func isDefaultTargetBuildCache(value *TargetBuildCache) bool {
	return value == nil || value.IsDefault()
}

// unmarshalTargetBuildCacheJSON reads an optional build cache table. "No
// overrides" has one representation. A section with every field unset, which
// both a hand-written file and the setup editor can produce, reads back as no
// section at all.
func unmarshalTargetBuildCacheJSON(data []byte) (*TargetBuildCache, error) {
	if strings.TrimSpace(string(data)) == "null" {
		return nil, nil
	}
	var cache TargetBuildCache
	if err := json.Unmarshal(data, &cache); err != nil {
		return nil, err
	}
	if cache.IsDefault() {
		return nil, nil
	}
	return &cache, nil
}

// ParseBuildCacheSize is the byte count of an mbx size string: digits and an
// optional unit, the same spellings mbx's own bytesize parser accepts. `20GB`
// and `20GiB` are different numbers, so the unit is kept exactly as written.
func ParseBuildCacheSize(value string) (uint64, bool) {
	value = strings.TrimSpace(value)
	digits := 0
	for digits < len(value) && value[digits] >= '0' && value[digits] <= '9' {
		digits++
	}
	if digits == 0 {
		return 0, false
	}
	count, err := strconv.ParseUint(value[:digits], 10, 64)
	if err != nil {
		return 0, false
	}
	var multiplier uint64
	switch strings.TrimLeftFunc(value[digits:], unicode.IsSpace) {
	case "", "B":
		multiplier = 1
	case "KB":
		multiplier = 1_000
	case "MB":
		multiplier = 1_000_000
	case "GB":
		multiplier = 1_000_000_000
	case "TB":
		multiplier = 1_000_000_000_000
	case "KiB":
		multiplier = 1 << 10
	case "MiB":
		multiplier = 1 << 20
	case "GiB":
		multiplier = 1 << 30
	case "TiB":
		multiplier = 1 << 40
	default:
		return 0, false
	}
	if count != 0 && count > math.MaxUint64/multiplier {
		return 0, false
	}
	return count * multiplier, true
}

// BuildCacheSizeGigabytes is an mbx size string as a whole number of
// gigabytes, rounded to the nearest one. The setup editor measures the cache
// in GB, so a value written in any other unit still has a number to show.
func BuildCacheSizeGigabytes(value string) (uint64, bool) {
	const gigabyte uint64 = 1_000_000_000
	bytes, ok := ParseBuildCacheSize(value)
	if !ok {
		return 0, false
	}
	sum := bytes + gigabyte/2
	if sum < bytes {
		sum = ^uint64(0)
	}
	return sum / gigabyte, true
}

// BuildCacheSizeFromGigabytes is the size string stored for a whole number of
// gigabytes.
func BuildCacheSizeFromGigabytes(gigabytes uint64) string {
	return strconv.FormatUint(gigabytes, 10) + "GB"
}

// BuildCacheConfigurationDirectory is the shared machine policy, reachable
// through the cache mount in every container.
func BuildCacheConfigurationDirectory(directory string) string {
	return filepath.Join(directory, ".mjolnir/config/mbx")
}

// defaultContainerImage is the image a container target runs when its table
// names none: the reference image CI publishes, which first-run setup also
// writes.
func defaultContainerImage() string { return DefaultSessionImage }

// isDefaultImage reports whether a container image is exactly this build's
// default. A template that keeps the default writes no `image` line, so an
// upgraded install follows the image built for its own version.
func isDefaultImage(image string) bool { return image == DefaultSessionImage }

// containerTemplateKeys is every key a container runtime's table may carry
// beside `kind` and `machine`. A target table flattens ContainerTemplate into
// a tagged enum, where serde cannot refuse unknown keys, so the loader
// compares the table against this list to name the keys it ignores (R4-6).
var containerTemplateKeys = [8]string{
	"image", "pull_policy", "platform", "cpus", "memory", "environment", "workspace_storage", "build_cache",
}

type ContainerTemplate struct {
	Image      string          `json:"image" toml:"image"`
	PullPolicy ImagePullPolicy `json:"pull_policy" toml:"pull_policy"`
	Platform   *string         `json:"platform,omitempty" toml:"platform,omitempty"`
	CPUs       *string         `json:"cpus,omitempty" toml:"cpus,omitempty"`
	Memory     *string         `json:"memory,omitempty" toml:"memory,omitempty"`
	// Environment is the environment inside the container. An entry may name
	// a secret instead of holding it; see Environment.
	Environment      Environment            `json:"environment" toml:"environment"`
	WorkspaceStorage PodmanWorkspaceStorage `json:"workspace_storage" toml:"workspace_storage"`
	// BuildCache is the per-target mbx build cache overrides.
	BuildCache *TargetBuildCache `json:"build_cache,omitempty" toml:"build_cache,omitempty"`
}

// DefaultHostContainerSize is the size this template's `cpus` and `memory`
// configure, when both are set, readable and positive.
func (c ContainerTemplate) DefaultHostContainerSize() *HostContainerSize {
	if c.CPUs == nil || c.Memory == nil {
		return nil
	}
	cpus, ok := parseRustUnsigned(strings.TrimSpace(*c.CPUs))
	if !ok {
		return nil
	}
	memory, err := ParseContainerMemory(*c.Memory)
	if err != nil || cpus == 0 || memory == 0 {
		return nil
	}
	return &HostContainerSize{Cpus: cpus, MemoryBytes: memory}
}

func (c ContainerTemplate) MarshalJSON() ([]byte, error) {
	var object serdeObject
	object.fieldUnless(isDefaultImage(c.Image), "image", c.Image)
	object.fieldUnless(c.PullPolicy.isAuto(), "pull_policy", c.PullPolicy)
	object.fieldUnless(c.Platform == nil, "platform", c.Platform)
	object.fieldUnless(c.CPUs == nil, "cpus", c.CPUs)
	object.fieldUnless(c.Memory == nil, "memory", c.Memory)
	object.fieldUnless(c.Environment.IsEmpty(), "environment", c.Environment)
	object.fieldUnless(podmanWorkspaceStorageIsDefault(c.WorkspaceStorage), "workspace_storage", c.WorkspaceStorage)
	object.fieldUnless(isDefaultTargetBuildCache(c.BuildCache), "build_cache", c.BuildCache)
	return object.finish()
}

// UnmarshalJSON applies ContainerTemplate's defaults; it reads the members of
// a flattened table too, ignoring keys that are not its own.
func (c *ContainerTemplate) UnmarshalJSON(data []byte) error {
	fields, err := decodeObject(data)
	if err != nil {
		return err
	}
	value := ContainerTemplate{Image: defaultContainerImage(), PullPolicy: ImagePullPolicyAuto, WorkspaceStorage: PodmanWorkspaceStoragePodmanVolume{}}
	decoders := map[string]func(json.RawMessage) error{
		"image":       func(raw json.RawMessage) error { return json.Unmarshal(raw, &value.Image) },
		"pull_policy": func(raw json.RawMessage) error { return json.Unmarshal(raw, &value.PullPolicy) },
		"platform":    func(raw json.RawMessage) error { return json.Unmarshal(raw, &value.Platform) },
		"cpus":        func(raw json.RawMessage) error { return json.Unmarshal(raw, &value.CPUs) },
		"memory":      func(raw json.RawMessage) error { return json.Unmarshal(raw, &value.Memory) },
		"environment": func(raw json.RawMessage) error { return json.Unmarshal(raw, &value.Environment) },
		"workspace_storage": func(raw json.RawMessage) (err error) {
			value.WorkspaceStorage, err = UnmarshalPodmanWorkspaceStorageJSON(raw)
			return err
		},
		"build_cache": func(raw json.RawMessage) (err error) {
			value.BuildCache, err = unmarshalTargetBuildCacheJSON(raw)
			return err
		},
	}
	for _, key := range containerTemplateKeys {
		if raw, ok := fields[key]; ok {
			if err := decoders[key](raw); err != nil {
				return err
			}
		}
	}
	*c = value
	return nil
}

// PodmanWorkspaceStorage is where a Podman container keeps its workspace
// (tag = "kind", kebab-case). Fields of this type are read with
// UnmarshalPodmanWorkspaceStorageJSON.
type PodmanWorkspaceStorage interface {
	isPodmanWorkspaceStorage()
	MarshalJSON() ([]byte, error)
}

// PodmanWorkspaceStoragePodmanVolume is the default.
type PodmanWorkspaceStoragePodmanVolume struct{}

type PodmanWorkspaceStorageHostHelper struct {
	Root   string   `json:"root"`
	Helper []string `json:"helper"`
}

type PodmanWorkspaceStorageContainerLayer struct{}

func (PodmanWorkspaceStoragePodmanVolume) isPodmanWorkspaceStorage()   {}
func (PodmanWorkspaceStorageHostHelper) isPodmanWorkspaceStorage()     {}
func (PodmanWorkspaceStorageContainerLayer) isPodmanWorkspaceStorage() {}

func (PodmanWorkspaceStoragePodmanVolume) MarshalJSON() ([]byte, error) {
	return []byte(`{"kind":"podman-volume"}`), nil
}

func (s PodmanWorkspaceStorageHostHelper) MarshalJSON() ([]byte, error) {
	var object serdeObject
	object.field("kind", "host-helper")
	object.field("root", s.Root)
	object.field("helper", s.Helper)
	return object.finish()
}

func (PodmanWorkspaceStorageContainerLayer) MarshalJSON() ([]byte, error) {
	return []byte(`{"kind":"container-layer"}`), nil
}

// UnmarshalPodmanWorkspaceStorageJSON reads a PodmanWorkspaceStorage.
func UnmarshalPodmanWorkspaceStorageJSON(data []byte) (PodmanWorkspaceStorage, error) {
	kind, err := serdeTag(data, "kind")
	if err != nil {
		return nil, err
	}
	switch kind {
	case "podman-volume":
		return PodmanWorkspaceStoragePodmanVolume{}, nil
	case "container-layer":
		return PodmanWorkspaceStorageContainerLayer{}, nil
	case "host-helper":
		if err := requireFields(data, "root", "helper"); err != nil {
			return nil, err
		}
		var value PodmanWorkspaceStorageHostHelper
		if err := json.Unmarshal(data, &value); err != nil {
			return nil, err
		}
		return value, nil
	}
	return nil, serdeUnknownVariant(kind, "podman-volume", "host-helper", "container-layer")
}

func podmanWorkspaceStorageIsDefault(storage PodmanWorkspaceStorage) bool {
	_, ok := storage.(PodmanWorkspaceStoragePodmanVolume)
	return storage == nil || ok
}

func podmanWorkspaceStorageValidate(storage PodmanWorkspaceStorage, templateID string) error {
	helper, ok := storage.(PodmanWorkspaceStorageHostHelper)
	if !ok {
		return nil
	}
	if !IsAbsolute(helper.Root) {
		return fmt.Errorf("target template %s workspace storage root must be absolute", debugString(templateID))
	}
	if len(helper.Helper) == 0 || slicesContainsEmpty(helper.Helper) {
		return fmt.Errorf("target template %s workspace storage helper must contain non-empty arguments", debugString(templateID))
	}
	return nil
}

func slicesContainsEmpty(values []string) bool {
	for _, value := range values {
		if value == "" {
			return true
		}
	}
	return false
}

type ImagePullPolicy string

const (
	// ImagePullPolicyAuto is the default.
	ImagePullPolicyAuto    ImagePullPolicy = "auto"
	ImagePullPolicyAlways  ImagePullPolicy = "always"
	ImagePullPolicyNewer   ImagePullPolicy = "newer"
	ImagePullPolicyMissing ImagePullPolicy = "missing"
	ImagePullPolicyNever   ImagePullPolicy = "never"
)

func (p ImagePullPolicy) isAuto() bool { return p == ImagePullPolicyAuto || p == "" }

func (p *ImagePullPolicy) UnmarshalText(text []byte) error {
	switch ImagePullPolicy(text) {
	case ImagePullPolicyAuto, ImagePullPolicyAlways, ImagePullPolicyNewer, ImagePullPolicyMissing, ImagePullPolicyNever:
		*p = ImagePullPolicy(text)
		return nil
	}
	return serdeUnknownVariant(string(text), "auto", "always", "newer", "missing", "never")
}

func (c ContainerTemplate) validate(templateID string) error {
	if strings.TrimSpace(c.Image) == "" {
		return fmt.Errorf("target template %s has an empty container image", debugString(templateID))
	}
	if err := validateEnvironment(templateID, c.Environment); err != nil {
		return err
	}
	if c.BuildCache != nil {
		return c.BuildCache.validate(templateID)
	}
	return nil
}

type AwsAddressSource string

const (
	// AwsAddressSourcePublicDNS is the default.
	AwsAddressSourcePublicDNS  AwsAddressSource = "public-dns"
	AwsAddressSourcePublicIP   AwsAddressSource = "public-ip"
	AwsAddressSourcePrivateDNS AwsAddressSource = "private-dns"
	AwsAddressSourcePrivateIP  AwsAddressSource = "private-ip"
)

func (a *AwsAddressSource) UnmarshalText(text []byte) error {
	switch AwsAddressSource(text) {
	case AwsAddressSourcePublicDNS, AwsAddressSourcePublicIP, AwsAddressSourcePrivateDNS, AwsAddressSourcePrivateIP:
		*a = AwsAddressSource(text)
		return nil
	}
	return serdeUnknownVariant(string(text), "public-dns", "public-ip", "private-dns", "private-ip")
}

type SshConnection struct {
	// Host is an OpenSSH destination such as `builder.example.com` or an SSH
	// config alias.
	Host         string   `json:"host" toml:"host"`
	User         *string  `json:"user,omitempty" toml:"user,omitempty"`
	IdentityFile *string  `json:"identity_file,omitempty" toml:"identity_file,omitempty"`
	ExtraArgs    []string `json:"extra_args,omitempty" toml:"extra_args,omitempty"`
}

// UnmarshalJSON requires `host`, as serde does; it reads the members of a
// flattened table too.
func (s *SshConnection) UnmarshalJSON(data []byte) error {
	if err := requireFields(data, "host"); err != nil {
		return err
	}
	type plain SshConnection
	var value plain
	if err := json.Unmarshal(data, &value); err != nil {
		return err
	}
	*s = SshConnection(value)
	return nil
}

func (s SshConnection) validate(templateID string) error {
	if strings.TrimSpace(s.Host) == "" || strings.ContainsFunc(s.Host, unicode.IsSpace) {
		return fmt.Errorf("target template %s has an invalid SSH host", debugString(templateID))
	}
	if s.User != nil && (*s.User == "" || strings.ContainsFunc(*s.User, func(r rune) bool { return unicode.IsSpace(r) || r == '@' })) {
		return fmt.Errorf("target template %s has an invalid SSH user", debugString(templateID))
	}
	return nil
}

// DefaultWorkspacePrefix is where a new SSH machine keeps bare-runtime
// workspaces, relative to the login home. Setup writes it into the file for
// every machine it adds.
const DefaultWorkspacePrefix = ".local/share/mjolnir/workspaces"

// LegacyWorkspacePrefix is the directory used before the product was renamed.
// A machine whose file does not name a directory keeps this one, because its
// existing workspaces are there.
const LegacyWorkspacePrefix = ".local/share/hel/workspaces"

// defaultNamedMachinePrefix is the directory of a machine the file does not
// give one.
func defaultNamedMachinePrefix() string { return LegacyWorkspacePrefix }

// validateWorkspacePrefix rejects a remote workspace directory that would
// escape the login home or name the home itself. Shared by SSH machines and
// the targets that resolve from them so both report the same rule.
func validateWorkspacePrefix(id, workspacePrefix string) error {
	unsafe := workspacePrefix == ""
	for _, component := range pathComponents(workspacePrefix) {
		if component.kind == pathComponentParentDir {
			unsafe = true
		}
	}
	switch workspacePrefix {
	case "/", ".", "~", "~/":
		unsafe = true
	}
	if unsafe {
		return fmt.Errorf("%s has an unsafe workspace prefix", debugString(id))
	}
	return nil
}

// TargetTemplate is a host and a runtime fused together (tag = "kind",
// kebab-case). Fields of this type are read with UnmarshalTargetTemplateJSON.
type TargetTemplate interface {
	isTargetTemplate()
	// LaunchRefusal is the one availability rule for runtime kinds retained
	// only to read history.
	LaunchRefusal() (string, bool)
	// EnsureReady reports whether a session can start on this target: every
	// environment reference its container names resolved. Like
	// HarnessProfile.EnsureReady, this is not part of configuration
	// validation, so a missing secret stops only this target's sessions.
	EnsureReady(id string) error
	// Container is the container this runtime starts, for the container
	// runtimes; nil otherwise. It is a copy: change a template's container
	// with withContainer.
	Container() *ContainerTemplate
	// KindName is the `kind` spelling used in configuration and on the wire.
	KindName() string
	// WithoutLaunchOnlySettings is a copy without the settings that only
	// matter when a session is launched.
	//
	// Build cache settings are resolved once while a session is provisioned
	// and stored on its record, so an active session does not depend on
	// them. Comparing templates without them lets setup edit a machine's
	// build cache while sessions are running.
	WithoutLaunchOnlySettings() TargetTemplate
	ExecutionPolicy() ExecutionPolicy
	// PermissionMode is the approval behavior of a target that runs the
	// harness directly on a machine, or false for a target whose container
	// or instance boundary replaces approvals. Localhost has no setting for
	// it: the harness always keeps its configured approvals there.
	PermissionMode() (PermissionMode, bool)
	// IsYolo reports whether the harness runs without approvals on this
	// target: the execution policy a new session records and its worker
	// launch enforces. Containers and SSH bare targets with
	// `permissions = "yolo"` qualify.
	IsYolo() bool
	// withContainer is this template with its container replaced (Rust's
	// container_mut); unchanged for a template without one.
	withContainer(container ContainerTemplate) TargetTemplate
	validate(id string) error
	MarshalJSON() ([]byte, error)
}

type TargetTemplateLocalBare struct{}

type TargetTemplateLocalPodman struct {
	ContainerTemplate ContainerTemplate
}

type TargetTemplateLocalDocker struct {
	ContainerTemplate ContainerTemplate
}

type TargetTemplateAppleContainer struct {
	ContainerTemplate ContainerTemplate
}

type TargetTemplateAwsEc2 struct {
	AWSProfile            *string          `json:"aws_profile,omitempty"`
	Region                string           `json:"region"`
	LaunchTemplate        string           `json:"launch_template"`
	LaunchTemplateVersion *string          `json:"launch_template_version,omitempty"`
	SSHUser               string           `json:"ssh_user"`
	AddressSource         AwsAddressSource `json:"address_source"`
	IdentityFile          *string          `json:"identity_file,omitempty"`
	SSHArgs               []string         `json:"ssh_args,omitempty"`
}

type TargetTemplateSshBare struct {
	SSH             SshConnection
	Permissions     PermissionMode
	WorkspacePrefix string
}

type TargetTemplateSshPodman struct {
	SSH               SshConnection
	ContainerTemplate ContainerTemplate
}

type TargetTemplateSshDocker struct {
	SSH               SshConnection
	ContainerTemplate ContainerTemplate
}

type TargetTemplateSshAppleContainer struct {
	SSH               SshConnection
	ContainerTemplate ContainerTemplate
}

// TargetTemplateSshNitro is an AWS Nitro Sandbox: one Firecracker microVM per
// session on an EC2 host reached over SSH, driven by `mj-worker microvm` as
// root.
type TargetTemplateSshNitro struct {
	SSH               SshConnection
	ContainerTemplate ContainerTemplate
}

func targetTemplateLaunchRefusal(t TargetTemplate) (string, bool) {
	if _, ok := t.(TargetTemplateAwsEc2); ok {
		return LegacyEC2TargetNotice, true
	}
	return "", false
}

func targetTemplateEnsureReady(t TargetTemplate, id string) error {
	container := t.Container()
	if container == nil {
		return nil
	}
	if err := container.Environment.EnsureResolved(); err != nil {
		return RefusalPrecondition(fmt.Sprintf("target %s: %s", debugString(id), err))
	}
	return nil
}

func targetTemplateContainer(t TargetTemplate) *ContainerTemplate {
	var container ContainerTemplate
	switch t := t.(type) {
	case TargetTemplateLocalPodman:
		container = t.ContainerTemplate
	case TargetTemplateLocalDocker:
		container = t.ContainerTemplate
	case TargetTemplateAppleContainer:
		container = t.ContainerTemplate
	case TargetTemplateSshPodman:
		container = t.ContainerTemplate
	case TargetTemplateSshDocker:
		container = t.ContainerTemplate
	case TargetTemplateSshAppleContainer:
		container = t.ContainerTemplate
	case TargetTemplateSshNitro:
		container = t.ContainerTemplate
	default:
		return nil
	}
	return &container
}

func targetTemplateWithContainer(t TargetTemplate, container ContainerTemplate) TargetTemplate {
	switch t := t.(type) {
	case TargetTemplateLocalPodman:
		t.ContainerTemplate = container
		return t
	case TargetTemplateLocalDocker:
		t.ContainerTemplate = container
		return t
	case TargetTemplateAppleContainer:
		t.ContainerTemplate = container
		return t
	case TargetTemplateSshPodman:
		t.ContainerTemplate = container
		return t
	case TargetTemplateSshDocker:
		t.ContainerTemplate = container
		return t
	case TargetTemplateSshAppleContainer:
		t.ContainerTemplate = container
		return t
	case TargetTemplateSshNitro:
		t.ContainerTemplate = container
		return t
	}
	return t
}

func targetTemplateKindName(t TargetTemplate) string {
	switch t.(type) {
	case TargetTemplateLocalBare:
		return "local-bare"
	case TargetTemplateLocalPodman:
		return "local-podman"
	case TargetTemplateLocalDocker:
		return "local-docker"
	case TargetTemplateAppleContainer:
		return "apple-container"
	case TargetTemplateAwsEc2:
		return "aws-ec2"
	case TargetTemplateSshBare:
		return "ssh-bare"
	case TargetTemplateSshPodman:
		return "ssh-podman"
	case TargetTemplateSshDocker:
		return "ssh-docker"
	case TargetTemplateSshAppleContainer:
		return "ssh-apple-container"
	case TargetTemplateSshNitro:
		return "ssh-nitro"
	}
	panic(fmt.Sprintf("unknown target template %T", t))
}

func targetTemplateWithoutLaunchOnlySettings(t TargetTemplate) TargetTemplate {
	container := t.Container()
	if container == nil {
		return t
	}
	stripped := *container
	stripped.BuildCache = nil
	return t.withContainer(stripped)
}

func targetTemplateExecutionPolicy(t TargetTemplate) ExecutionPolicy {
	if mode, ok := t.PermissionMode(); ok {
		return mode.ExecutionPolicy()
	}
	return ExecutionPolicyUnconstrained
}

func targetTemplatePermissionMode(t TargetTemplate) (PermissionMode, bool) {
	switch t := t.(type) {
	case TargetTemplateLocalBare:
		return PermissionModeGuardian, true
	case TargetTemplateSshBare:
		return t.Permissions, true
	}
	return "", false
}

func targetTemplateIsYolo(t TargetTemplate) bool {
	return t.ExecutionPolicy() == ExecutionPolicyUnconstrained
}

func targetTemplateValidate(t TargetTemplate, id string) error {
	if err := ValidateID("target template", id); err != nil {
		return err
	}
	onlyPodman := func(container ContainerTemplate) error {
		if !podmanWorkspaceStorageIsDefault(container.WorkspaceStorage) {
			return fmt.Errorf("target template %s workspace storage is only supported by Podman", debugString(id))
		}
		return nil
	}
	switch t := t.(type) {
	case TargetTemplateLocalBare:
		return nil
	case TargetTemplateLocalPodman:
		if err := t.ContainerTemplate.validate(id); err != nil {
			return err
		}
		return podmanWorkspaceStorageValidate(t.ContainerTemplate.WorkspaceStorage, id)
	case TargetTemplateLocalDocker:
		if err := t.ContainerTemplate.validate(id); err != nil {
			return err
		}
		return onlyPodman(t.ContainerTemplate)
	case TargetTemplateAppleContainer:
		if err := t.ContainerTemplate.validate(id); err != nil {
			return err
		}
		return onlyPodman(t.ContainerTemplate)
	case TargetTemplateAwsEc2:
		if configBlank(t.Region) || configBlank(t.LaunchTemplate) || configBlank(t.SSHUser) {
			return fmt.Errorf("AWS target template %s requires region, launch_template, and ssh_user", debugString(id))
		}
		if t.AWSProfile != nil && *t.AWSProfile == "" || t.LaunchTemplateVersion != nil && *t.LaunchTemplateVersion == "" {
			return fmt.Errorf("AWS target template %s contains an empty optional value", debugString(id))
		}
		return nil
	case TargetTemplateSshBare:
		if err := t.SSH.validate(id); err != nil {
			return err
		}
		return validateWorkspacePrefix("target template "+id, t.WorkspacePrefix)
	case TargetTemplateSshDocker:
		if err := t.SSH.validate(id); err != nil {
			return err
		}
		if err := t.ContainerTemplate.validate(id); err != nil {
			return err
		}
		return onlyPodman(t.ContainerTemplate)
	case TargetTemplateSshPodman:
		if err := t.SSH.validate(id); err != nil {
			return err
		}
		if err := t.ContainerTemplate.validate(id); err != nil {
			return err
		}
		return podmanWorkspaceStorageValidate(t.ContainerTemplate.WorkspaceStorage, id)
	// A remote Apple container runtime accepts exactly what the local one
	// does: the engine is the same, only its host differs.
	case TargetTemplateSshAppleContainer:
		if err := t.SSH.validate(id); err != nil {
			return err
		}
		if err := t.ContainerTemplate.validate(id); err != nil {
			return err
		}
		return onlyPodman(t.ContainerTemplate)
	// A Nitro sandbox keeps its workspace on a named volume the engine
	// creates; no other storage shape exists.
	case TargetTemplateSshNitro:
		if err := t.SSH.validate(id); err != nil {
			return err
		}
		if err := t.ContainerTemplate.validate(id); err != nil {
			return err
		}
		if !podmanWorkspaceStorageIsDefault(t.ContainerTemplate.WorkspaceStorage) {
			return errors.New("target template " + debugString(id) +
				" workspace storage is only supported by Podman; a Nitro sandbox always uses a volume")
		}
		return nil
	}
	panic(fmt.Sprintf("unknown target template %T", t))
}

// marshalTargetTemplateJSON writes the kind tag, then the variant's fields
// with its SSH connection and container flattened in place.
func marshalTargetTemplateJSON(t TargetTemplate) ([]byte, error) {
	var object serdeObject
	object.field("kind", t.KindName())
	switch t := t.(type) {
	case TargetTemplateLocalBare:
	case TargetTemplateLocalPodman:
		object.flatten(t.ContainerTemplate)
	case TargetTemplateLocalDocker:
		object.flatten(t.ContainerTemplate)
	case TargetTemplateAppleContainer:
		object.flatten(t.ContainerTemplate)
	case TargetTemplateAwsEc2:
		object.fieldUnless(t.AWSProfile == nil, "aws_profile", t.AWSProfile)
		object.field("region", t.Region)
		object.field("launch_template", t.LaunchTemplate)
		object.fieldUnless(t.LaunchTemplateVersion == nil, "launch_template_version", t.LaunchTemplateVersion)
		object.field("ssh_user", t.SSHUser)
		object.field("address_source", t.AddressSource)
		object.fieldUnless(t.IdentityFile == nil, "identity_file", t.IdentityFile)
		object.fieldUnless(len(t.SSHArgs) == 0, "ssh_args", t.SSHArgs)
	case TargetTemplateSshBare:
		object.flatten(t.SSH)
		object.field("permissions", t.Permissions)
		object.field("workspace_prefix", t.WorkspacePrefix)
	case TargetTemplateSshPodman:
		object.flatten(t.SSH)
		object.flatten(t.ContainerTemplate)
	case TargetTemplateSshDocker:
		object.flatten(t.SSH)
		object.flatten(t.ContainerTemplate)
	case TargetTemplateSshAppleContainer:
		object.flatten(t.SSH)
		object.flatten(t.ContainerTemplate)
	case TargetTemplateSshNitro:
		object.flatten(t.SSH)
		object.flatten(t.ContainerTemplate)
	}
	return object.finish()
}

// UnmarshalTargetTemplateJSON reads a TargetTemplate.
func UnmarshalTargetTemplateJSON(data []byte) (TargetTemplate, error) {
	kind, err := serdeTag(data, "kind")
	if err != nil {
		return nil, err
	}
	container := func() (ContainerTemplate, error) {
		var value ContainerTemplate
		err := json.Unmarshal(data, &value)
		return value, err
	}
	ssh := func() (SshConnection, error) {
		var value SshConnection
		err := json.Unmarshal(data, &value)
		return value, err
	}
	sshContainer := func() (SshConnection, ContainerTemplate, error) {
		connection, err := ssh()
		if err != nil {
			return SshConnection{}, ContainerTemplate{}, err
		}
		value, err := container()
		return connection, value, err
	}
	switch kind {
	case "local-bare":
		return TargetTemplateLocalBare{}, nil
	case "local-podman":
		value, err := container()
		return TargetTemplateLocalPodman{ContainerTemplate: value}, err
	case "local-docker":
		value, err := container()
		return TargetTemplateLocalDocker{ContainerTemplate: value}, err
	case "apple-container":
		value, err := container()
		return TargetTemplateAppleContainer{ContainerTemplate: value}, err
	case "aws-ec2":
		if err := requireFields(data, "region", "launch_template", "ssh_user"); err != nil {
			return nil, err
		}
		value := TargetTemplateAwsEc2{AddressSource: AwsAddressSourcePublicDNS}
		err := json.Unmarshal(data, &value)
		return value, err
	case "ssh-bare":
		connection, err := ssh()
		if err != nil {
			return nil, err
		}
		fields, err := decodeObject(data)
		if err != nil {
			return nil, err
		}
		raw, ok := fields["permissions"]
		if !ok {
			return nil, missingField("permissions")
		}
		value := TargetTemplateSshBare{SSH: connection, WorkspacePrefix: defaultNamedMachinePrefix()}
		if err := json.Unmarshal(raw, &value.Permissions); err != nil {
			return nil, err
		}
		if prefix, ok := fields["workspace_prefix"]; ok {
			if err := json.Unmarshal(prefix, &value.WorkspacePrefix); err != nil {
				return nil, err
			}
		}
		return value, nil
	case "ssh-podman":
		connection, value, err := sshContainer()
		return TargetTemplateSshPodman{SSH: connection, ContainerTemplate: value}, err
	case "ssh-docker":
		connection, value, err := sshContainer()
		return TargetTemplateSshDocker{SSH: connection, ContainerTemplate: value}, err
	case "ssh-apple-container":
		connection, value, err := sshContainer()
		return TargetTemplateSshAppleContainer{SSH: connection, ContainerTemplate: value}, err
	case "ssh-nitro":
		connection, value, err := sshContainer()
		return TargetTemplateSshNitro{SSH: connection, ContainerTemplate: value}, err
	}
	return nil, serdeUnknownVariant(kind, "local-bare", "local-podman", "local-docker", "apple-container",
		"aws-ec2", "ssh-bare", "ssh-podman", "ssh-docker", "ssh-apple-container", "ssh-nitro")
}

// ResourceAllocationKindFrom is From<&TargetTemplate> for
// ResourceAllocationKind.
func ResourceAllocationKindFrom(template TargetTemplate) ResourceAllocationKind {
	switch template.(type) {
	case TargetTemplateLocalPodman, TargetTemplateLocalDocker, TargetTemplateAppleContainer, TargetTemplateSshPodman,
		TargetTemplateSshDocker, TargetTemplateSshAppleContainer, TargetTemplateSshNitro:
		return ResourceAllocationKindContainer
	case TargetTemplateAwsEc2:
		return ResourceAllocationKindAwsEc2
	case TargetTemplateLocalBare, TargetTemplateSshBare:
		return ResourceAllocationKindFixed
	}
	panic(fmt.Sprintf("unknown target template %T", template))
}

// IsBareProjectTarget reports whether template hosts a raw project checkout
// directly on its machine, with no managed workspace. Bare targets take a
// project directory instead of a bundle.
func IsBareProjectTarget(template TargetTemplate) bool {
	switch template.(type) {
	case TargetTemplateLocalBare, TargetTemplateSshBare:
		return true
	}
	return false
}

// IsContainerTarget reports whether sessions on template run inside a
// container Hel creates, and so work in a container workspace rather than a
// host or instance directory.
func IsContainerTarget(template TargetTemplate) bool {
	return template.Container() != nil
}

// ContainerMemoryIsHardLimit reports whether a container's RAM size is a hard
// admission limit on its host.
//
// Apple's `container` runs each session in its own VM, and the macOS
// Virtualization framework does not return memory the guest frees, so a
// long-lived session's footprint grows toward its configured size. A Nitro
// sandbox takes its memory from a pool reserved at boot, which cannot be
// overcommitted. Every other runtime's limit is a cgroup ceiling that an idle
// container does not use, so its host may be overcommitted with a warning.
func ContainerMemoryIsHardLimit(template TargetTemplate) bool {
	switch template.(type) {
	case TargetTemplateAppleContainer, TargetTemplateSshAppleContainer, TargetTemplateSshNitro:
		return true
	}
	return false
}

// MountHistoryHost is the host name that prompt-history mounts on template
// should be filed under, or false if the target does not support attached
// mounts.
func MountHistoryHost(template TargetTemplate) (string, bool) {
	switch template := template.(type) {
	case TargetTemplateLocalPodman, TargetTemplateLocalDocker, TargetTemplateAppleContainer, TargetTemplateAwsEc2:
		return "local", true
	case TargetTemplateSshPodman:
		return template.SSH.Host, true
	case TargetTemplateSshDocker:
		return template.SSH.Host, true
	case TargetTemplateSshAppleContainer:
		return template.SSH.Host, true
	case TargetTemplateSshNitro:
		return template.SSH.Host, true
	}
	return "", false
}

// OffersAttachedDirectories reports whether a launch surface should offer
// attached host directories for template. A Nitro sandbox is a VM that cannot
// attach one, but a request that carries attachments (an API call, a resumed
// session) is still accepted: the launch drops them and says so, instead of
// refusing.
func OffersAttachedDirectories(template TargetTemplate) bool {
	_, ok := MountHistoryHost(template)
	_, nitro := template.(TargetTemplateSshNitro)
	return ok && !nitro
}

// ProjectHistoryHost is the host key whose project-directory history belongs
// to template. Unlike mount history, raw bare targets keep a recent project
// checkout list because their launch wizard selects a directory instead of an
// attachment.
func ProjectHistoryHost(template TargetTemplate) (string, bool) {
	switch template := template.(type) {
	case TargetTemplateLocalBare:
		return "local", true
	case TargetTemplateSshBare:
		return template.SSH.Host, true
	}
	return "", false
}

// ContainerSizeHost is the stable physical-host key for reusable container
// CPU and memory defaults.
func ContainerSizeHost(template TargetTemplate) (string, bool) {
	switch template := template.(type) {
	case TargetTemplateLocalPodman, TargetTemplateLocalDocker, TargetTemplateAppleContainer:
		return "local", true
	case TargetTemplateSshPodman:
		return template.SSH.Host, true
	case TargetTemplateSshDocker:
		return template.SSH.Host, true
	case TargetTemplateSshAppleContainer:
		return template.SSH.Host, true
	case TargetTemplateSshNitro:
		return template.SSH.Host, true
	}
	return "", false
}

func validateEnvironment(owner string, environment Environment) error {
	// The entries as written, so one whose reference did not resolve is
	// checked too.
	for key := range environment.Sources() {
		if strings.TrimSpace(key) == "" || strings.Contains(key, "=") {
			return fmt.Errorf("%s contains an invalid environment variable name", debugString(owner))
		}
	}
	return nil
}

// requireFields is serde's missing-field check for required members.
func requireFields(data []byte, names ...string) error {
	fields, err := decodeObject(data)
	if err != nil {
		return err
	}
	for _, name := range names {
		if _, ok := fields[name]; !ok {
			return missingField(name)
		}
	}
	return nil
}

// serdeTag is the string value of an internally tagged enum's tag member.
func serdeTag(data []byte, tag string) (string, error) {
	fields, err := decodeObject(data)
	if err != nil {
		return "", err
	}
	raw, ok := fields[tag]
	if !ok {
		return "", missingField(tag)
	}
	var value string
	if err := json.Unmarshal(raw, &value); err != nil {
		return "", fmt.Errorf("invalid type: %s, expected variant identifier", serdeUnexpected(raw))
	}
	return value, nil
}

// Method sets of the TargetTemplate variants, each delegating to the one
// function that decides it for every variant.

func (TargetTemplateLocalBare) isTargetTemplate()         {}
func (TargetTemplateLocalPodman) isTargetTemplate()       {}
func (TargetTemplateLocalDocker) isTargetTemplate()       {}
func (TargetTemplateAppleContainer) isTargetTemplate()    {}
func (TargetTemplateAwsEc2) isTargetTemplate()            {}
func (TargetTemplateSshBare) isTargetTemplate()           {}
func (TargetTemplateSshPodman) isTargetTemplate()         {}
func (TargetTemplateSshDocker) isTargetTemplate()         {}
func (TargetTemplateSshAppleContainer) isTargetTemplate() {}
func (TargetTemplateSshNitro) isTargetTemplate()          {}

func (t TargetTemplateLocalBare) LaunchRefusal() (string, bool) {
	return targetTemplateLaunchRefusal(t)
}
func (t TargetTemplateLocalBare) EnsureReady(id string) error {
	return targetTemplateEnsureReady(t, id)
}
func (t TargetTemplateLocalBare) Container() *ContainerTemplate { return targetTemplateContainer(t) }
func (t TargetTemplateLocalBare) KindName() string              { return targetTemplateKindName(t) }
func (t TargetTemplateLocalBare) WithoutLaunchOnlySettings() TargetTemplate {
	return targetTemplateWithoutLaunchOnlySettings(t)
}
func (t TargetTemplateLocalBare) ExecutionPolicy() ExecutionPolicy {
	return targetTemplateExecutionPolicy(t)
}
func (t TargetTemplateLocalBare) PermissionMode() (PermissionMode, bool) {
	return targetTemplatePermissionMode(t)
}
func (t TargetTemplateLocalBare) IsYolo() bool { return targetTemplateIsYolo(t) }
func (t TargetTemplateLocalBare) withContainer(container ContainerTemplate) TargetTemplate {
	return targetTemplateWithContainer(t, container)
}
func (t TargetTemplateLocalBare) validate(id string) error     { return targetTemplateValidate(t, id) }
func (t TargetTemplateLocalBare) MarshalJSON() ([]byte, error) { return marshalTargetTemplateJSON(t) }

func (t TargetTemplateLocalPodman) LaunchRefusal() (string, bool) {
	return targetTemplateLaunchRefusal(t)
}
func (t TargetTemplateLocalPodman) EnsureReady(id string) error {
	return targetTemplateEnsureReady(t, id)
}
func (t TargetTemplateLocalPodman) Container() *ContainerTemplate { return targetTemplateContainer(t) }
func (t TargetTemplateLocalPodman) KindName() string              { return targetTemplateKindName(t) }
func (t TargetTemplateLocalPodman) WithoutLaunchOnlySettings() TargetTemplate {
	return targetTemplateWithoutLaunchOnlySettings(t)
}
func (t TargetTemplateLocalPodman) ExecutionPolicy() ExecutionPolicy {
	return targetTemplateExecutionPolicy(t)
}
func (t TargetTemplateLocalPodman) PermissionMode() (PermissionMode, bool) {
	return targetTemplatePermissionMode(t)
}
func (t TargetTemplateLocalPodman) IsYolo() bool { return targetTemplateIsYolo(t) }
func (t TargetTemplateLocalPodman) withContainer(container ContainerTemplate) TargetTemplate {
	return targetTemplateWithContainer(t, container)
}
func (t TargetTemplateLocalPodman) validate(id string) error     { return targetTemplateValidate(t, id) }
func (t TargetTemplateLocalPodman) MarshalJSON() ([]byte, error) { return marshalTargetTemplateJSON(t) }

func (t TargetTemplateLocalDocker) LaunchRefusal() (string, bool) {
	return targetTemplateLaunchRefusal(t)
}
func (t TargetTemplateLocalDocker) EnsureReady(id string) error {
	return targetTemplateEnsureReady(t, id)
}
func (t TargetTemplateLocalDocker) Container() *ContainerTemplate { return targetTemplateContainer(t) }
func (t TargetTemplateLocalDocker) KindName() string              { return targetTemplateKindName(t) }
func (t TargetTemplateLocalDocker) WithoutLaunchOnlySettings() TargetTemplate {
	return targetTemplateWithoutLaunchOnlySettings(t)
}
func (t TargetTemplateLocalDocker) ExecutionPolicy() ExecutionPolicy {
	return targetTemplateExecutionPolicy(t)
}
func (t TargetTemplateLocalDocker) PermissionMode() (PermissionMode, bool) {
	return targetTemplatePermissionMode(t)
}
func (t TargetTemplateLocalDocker) IsYolo() bool { return targetTemplateIsYolo(t) }
func (t TargetTemplateLocalDocker) withContainer(container ContainerTemplate) TargetTemplate {
	return targetTemplateWithContainer(t, container)
}
func (t TargetTemplateLocalDocker) validate(id string) error     { return targetTemplateValidate(t, id) }
func (t TargetTemplateLocalDocker) MarshalJSON() ([]byte, error) { return marshalTargetTemplateJSON(t) }

func (t TargetTemplateAppleContainer) LaunchRefusal() (string, bool) {
	return targetTemplateLaunchRefusal(t)
}
func (t TargetTemplateAppleContainer) EnsureReady(id string) error {
	return targetTemplateEnsureReady(t, id)
}
func (t TargetTemplateAppleContainer) Container() *ContainerTemplate {
	return targetTemplateContainer(t)
}
func (t TargetTemplateAppleContainer) KindName() string { return targetTemplateKindName(t) }
func (t TargetTemplateAppleContainer) WithoutLaunchOnlySettings() TargetTemplate {
	return targetTemplateWithoutLaunchOnlySettings(t)
}
func (t TargetTemplateAppleContainer) ExecutionPolicy() ExecutionPolicy {
	return targetTemplateExecutionPolicy(t)
}
func (t TargetTemplateAppleContainer) PermissionMode() (PermissionMode, bool) {
	return targetTemplatePermissionMode(t)
}
func (t TargetTemplateAppleContainer) IsYolo() bool { return targetTemplateIsYolo(t) }
func (t TargetTemplateAppleContainer) withContainer(container ContainerTemplate) TargetTemplate {
	return targetTemplateWithContainer(t, container)
}
func (t TargetTemplateAppleContainer) validate(id string) error { return targetTemplateValidate(t, id) }
func (t TargetTemplateAppleContainer) MarshalJSON() ([]byte, error) {
	return marshalTargetTemplateJSON(t)
}

func (t TargetTemplateAwsEc2) LaunchRefusal() (string, bool) { return targetTemplateLaunchRefusal(t) }
func (t TargetTemplateAwsEc2) EnsureReady(id string) error   { return targetTemplateEnsureReady(t, id) }
func (t TargetTemplateAwsEc2) Container() *ContainerTemplate { return targetTemplateContainer(t) }
func (t TargetTemplateAwsEc2) KindName() string              { return targetTemplateKindName(t) }
func (t TargetTemplateAwsEc2) WithoutLaunchOnlySettings() TargetTemplate {
	return targetTemplateWithoutLaunchOnlySettings(t)
}
func (t TargetTemplateAwsEc2) ExecutionPolicy() ExecutionPolicy {
	return targetTemplateExecutionPolicy(t)
}
func (t TargetTemplateAwsEc2) PermissionMode() (PermissionMode, bool) {
	return targetTemplatePermissionMode(t)
}
func (t TargetTemplateAwsEc2) IsYolo() bool { return targetTemplateIsYolo(t) }
func (t TargetTemplateAwsEc2) withContainer(container ContainerTemplate) TargetTemplate {
	return targetTemplateWithContainer(t, container)
}
func (t TargetTemplateAwsEc2) validate(id string) error     { return targetTemplateValidate(t, id) }
func (t TargetTemplateAwsEc2) MarshalJSON() ([]byte, error) { return marshalTargetTemplateJSON(t) }

func (t TargetTemplateSshBare) LaunchRefusal() (string, bool) { return targetTemplateLaunchRefusal(t) }
func (t TargetTemplateSshBare) EnsureReady(id string) error   { return targetTemplateEnsureReady(t, id) }
func (t TargetTemplateSshBare) Container() *ContainerTemplate { return targetTemplateContainer(t) }
func (t TargetTemplateSshBare) KindName() string              { return targetTemplateKindName(t) }
func (t TargetTemplateSshBare) WithoutLaunchOnlySettings() TargetTemplate {
	return targetTemplateWithoutLaunchOnlySettings(t)
}
func (t TargetTemplateSshBare) ExecutionPolicy() ExecutionPolicy {
	return targetTemplateExecutionPolicy(t)
}
func (t TargetTemplateSshBare) PermissionMode() (PermissionMode, bool) {
	return targetTemplatePermissionMode(t)
}
func (t TargetTemplateSshBare) IsYolo() bool { return targetTemplateIsYolo(t) }
func (t TargetTemplateSshBare) withContainer(container ContainerTemplate) TargetTemplate {
	return targetTemplateWithContainer(t, container)
}
func (t TargetTemplateSshBare) validate(id string) error     { return targetTemplateValidate(t, id) }
func (t TargetTemplateSshBare) MarshalJSON() ([]byte, error) { return marshalTargetTemplateJSON(t) }

func (t TargetTemplateSshPodman) LaunchRefusal() (string, bool) {
	return targetTemplateLaunchRefusal(t)
}
func (t TargetTemplateSshPodman) EnsureReady(id string) error {
	return targetTemplateEnsureReady(t, id)
}
func (t TargetTemplateSshPodman) Container() *ContainerTemplate { return targetTemplateContainer(t) }
func (t TargetTemplateSshPodman) KindName() string              { return targetTemplateKindName(t) }
func (t TargetTemplateSshPodman) WithoutLaunchOnlySettings() TargetTemplate {
	return targetTemplateWithoutLaunchOnlySettings(t)
}
func (t TargetTemplateSshPodman) ExecutionPolicy() ExecutionPolicy {
	return targetTemplateExecutionPolicy(t)
}
func (t TargetTemplateSshPodman) PermissionMode() (PermissionMode, bool) {
	return targetTemplatePermissionMode(t)
}
func (t TargetTemplateSshPodman) IsYolo() bool { return targetTemplateIsYolo(t) }
func (t TargetTemplateSshPodman) withContainer(container ContainerTemplate) TargetTemplate {
	return targetTemplateWithContainer(t, container)
}
func (t TargetTemplateSshPodman) validate(id string) error     { return targetTemplateValidate(t, id) }
func (t TargetTemplateSshPodman) MarshalJSON() ([]byte, error) { return marshalTargetTemplateJSON(t) }

func (t TargetTemplateSshDocker) LaunchRefusal() (string, bool) {
	return targetTemplateLaunchRefusal(t)
}
func (t TargetTemplateSshDocker) EnsureReady(id string) error {
	return targetTemplateEnsureReady(t, id)
}
func (t TargetTemplateSshDocker) Container() *ContainerTemplate { return targetTemplateContainer(t) }
func (t TargetTemplateSshDocker) KindName() string              { return targetTemplateKindName(t) }
func (t TargetTemplateSshDocker) WithoutLaunchOnlySettings() TargetTemplate {
	return targetTemplateWithoutLaunchOnlySettings(t)
}
func (t TargetTemplateSshDocker) ExecutionPolicy() ExecutionPolicy {
	return targetTemplateExecutionPolicy(t)
}
func (t TargetTemplateSshDocker) PermissionMode() (PermissionMode, bool) {
	return targetTemplatePermissionMode(t)
}
func (t TargetTemplateSshDocker) IsYolo() bool { return targetTemplateIsYolo(t) }
func (t TargetTemplateSshDocker) withContainer(container ContainerTemplate) TargetTemplate {
	return targetTemplateWithContainer(t, container)
}
func (t TargetTemplateSshDocker) validate(id string) error     { return targetTemplateValidate(t, id) }
func (t TargetTemplateSshDocker) MarshalJSON() ([]byte, error) { return marshalTargetTemplateJSON(t) }

func (t TargetTemplateSshAppleContainer) LaunchRefusal() (string, bool) {
	return targetTemplateLaunchRefusal(t)
}
func (t TargetTemplateSshAppleContainer) EnsureReady(id string) error {
	return targetTemplateEnsureReady(t, id)
}
func (t TargetTemplateSshAppleContainer) Container() *ContainerTemplate {
	return targetTemplateContainer(t)
}
func (t TargetTemplateSshAppleContainer) KindName() string { return targetTemplateKindName(t) }
func (t TargetTemplateSshAppleContainer) WithoutLaunchOnlySettings() TargetTemplate {
	return targetTemplateWithoutLaunchOnlySettings(t)
}
func (t TargetTemplateSshAppleContainer) ExecutionPolicy() ExecutionPolicy {
	return targetTemplateExecutionPolicy(t)
}
func (t TargetTemplateSshAppleContainer) PermissionMode() (PermissionMode, bool) {
	return targetTemplatePermissionMode(t)
}
func (t TargetTemplateSshAppleContainer) IsYolo() bool { return targetTemplateIsYolo(t) }
func (t TargetTemplateSshAppleContainer) withContainer(container ContainerTemplate) TargetTemplate {
	return targetTemplateWithContainer(t, container)
}
func (t TargetTemplateSshAppleContainer) validate(id string) error {
	return targetTemplateValidate(t, id)
}
func (t TargetTemplateSshAppleContainer) MarshalJSON() ([]byte, error) {
	return marshalTargetTemplateJSON(t)
}

func (t TargetTemplateSshNitro) LaunchRefusal() (string, bool) { return targetTemplateLaunchRefusal(t) }
func (t TargetTemplateSshNitro) EnsureReady(id string) error   { return targetTemplateEnsureReady(t, id) }
func (t TargetTemplateSshNitro) Container() *ContainerTemplate { return targetTemplateContainer(t) }
func (t TargetTemplateSshNitro) KindName() string              { return targetTemplateKindName(t) }
func (t TargetTemplateSshNitro) WithoutLaunchOnlySettings() TargetTemplate {
	return targetTemplateWithoutLaunchOnlySettings(t)
}
func (t TargetTemplateSshNitro) ExecutionPolicy() ExecutionPolicy {
	return targetTemplateExecutionPolicy(t)
}
func (t TargetTemplateSshNitro) PermissionMode() (PermissionMode, bool) {
	return targetTemplatePermissionMode(t)
}
func (t TargetTemplateSshNitro) IsYolo() bool { return targetTemplateIsYolo(t) }
func (t TargetTemplateSshNitro) withContainer(container ContainerTemplate) TargetTemplate {
	return targetTemplateWithContainer(t, container)
}
func (t TargetTemplateSshNitro) validate(id string) error     { return targetTemplateValidate(t, id) }
func (t TargetTemplateSshNitro) MarshalJSON() ([]byte, error) { return marshalTargetTemplateJSON(t) }
