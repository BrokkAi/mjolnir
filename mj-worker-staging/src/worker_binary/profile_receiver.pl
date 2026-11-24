# This program travels with the controller. The profile store remains usable
# by older controllers and needs no change to the installed worker.
use strict;
use warnings;
use Digest::SHA;
use Fcntl qw(:flock F_SETFD);
use File::Basename qw(dirname);
use File::Copy qw(copy);
use File::Path qw(make_path remove_tree);
use File::Temp qw(tempdir);
use JSON::PP qw(decode_json);

my ($store, $destination) = @ARGV;
$store = "$ENV{HOME}/$store";
# Keep leases and pins on the object filesystem, even when the store itself
# is a separate mount. The legacy collector reserves .stage-* for pinned work.
my $operations = "$store/.stage-negotiated";
umask 0077;
make_path($store, $operations, "$operations/claims");
chmod(0700, $store, $operations) == 2 or die "secure profile store: $!";

# The metadata lock only protects decisions over names. It is never held
# while waiting for another transfer, receiving bytes, or copying a profile.
open(my $metadata, '>>', "$operations/metadata") or die "open profile store lock: $!";
flock($metadata, LOCK_EX) or die "lock profile store: $!";
my $work = tempdir('receive-XXXXXXXXXX', DIR => $operations);
my $lease_path = "$work/lease";
open(my $lease, '>>', $lease_path) or die "open profile receiver lease: $!";
flock($lease, LOCK_EX) or die "lock profile receiver lease: $!";
my ($operation) = $work =~ m{([^/]+)\z};
my $owner = "$operation\n";
my $written = syswrite($lease, $owner);
die "write profile receiver identity: $!" unless defined($written) && $written == length($owner);
# Tar may still be writing after a receiver is killed. Keep the operation
# lease in that child until it exits so collection cannot delete its files.
defined(fcntl($lease, F_SETFD, 0)) or die "share profile lease with tar: $!";
END { remove_tree($work) if defined $work }

# A copy that crossed filesystems and failed leaves no partial profile behind.
my $exdev_staging;
END { remove_tree($exdev_staging) if defined $exdev_staging }

sub copy_profile_entry {
    my ($source, $target) = @_;
    my @status = lstat($source) or die "inspect profile entry: $!";
    my $mode = $status[2] & 07777;
    if (-l _) {
        my $link = readlink($source);
        defined($link) or die "read profile symlink: $!";
        symlink($link, $target) or die "copy profile symlink: $!";
    } elsif (-d _) {
        mkdir($target) or die "copy profile directory: $!";
        copy_directory_contents($source, $target);
        chmod($mode, $target) or die "set copied profile directory mode: $!";
    } elsif (-f _) {
        copy($source, $target) or die "copy profile file: $!";
        chmod($mode, $target) or die "set copied profile file mode: $!";
    } else {
        die "unsupported profile entry type";
    }
}

sub copy_directory_contents {
    my ($source, $target) = @_;
    opendir(my $entries, $source) or die "list profile directory: $!";
    # A file may be named "0", which is false but defined.
    while (defined(my $entry = readdir($entries))) {
        next if $entry eq '.' || $entry eq '..';
        copy_profile_entry("$source/$entry", "$target/$entry");
    }
    closedir($entries);
}
mkdir "$work/pins" or die "create profile pins: $!";
flock($metadata, LOCK_UN) or die "unlock profile store: $!";

my @offer;
my %offered;
my $complete;
while (defined(my $line = <STDIN>)) {
    if ($line eq "\n") { $complete = 1; last }
    my ($key, $size) = $line =~ /\A([0-9a-f]{64}) (0|[1-9][0-9]*)\n\z/
        or die "invalid profile offer";
    die "duplicate profile offer" if exists $offered{$key};
    $offered{$key} = $size;
    push @offer, $key;
}
die "incomplete profile offer" unless $complete;

# Each claim name is a hard link to the receiver's locked operation lease.
# link creates the name exclusively and needs no per-object descriptor. Older
# receivers that lock the claim file itself still see the same locked inode.
sub claim_is_live {
    my ($path) = @_;
    if (!open(my $probe, '+<', $path)) {
        return 0 if $!{ENOENT};
        die "open profile claim for inspection: $!";
    } else {
        return 0 if flock($probe, LOCK_EX | LOCK_NB);
        return 1 if $!{EWOULDBLOCK};
        die "inspect profile claim: $!";
    }
}

sub pin_profile_object {
    my ($key) = @_;
    for my $name ($key, "$key.x") {
        my $blob = "$store/$name";
        my $pin = "$work/pins/$key";
        if (-f $blob && -s $blob == $offered{$key} && link($blob, $pin)) {
            utime(undef, undef, $pin) or die "touch profile content: $!";
            return 1;
        }
    }
    return 0;
}

flock($metadata, LOCK_EX) or die "lock profile store: $!";
my (%claims, @reply);
my $busy = 0;
for my $key (@offer) {
    if (pin_profile_object($key)) {
        push @reply, "present $key\n";
        next;
    }
    my $claim = "$operations/claims/$key";
    if (!link($lease_path, $claim)) {
        die "create profile claim: $!" unless $!{EEXIST};
        if (claim_is_live($claim)) {
            $busy = 1;
            push @reply, "busy $key\n";
            next;
        }
        # A legacy owner can publish without the metadata lock. If it
        # finished before our probe acquired the claim, use that publication.
        if (pin_profile_object($key)) {
            push @reply, "present $key\n";
            next;
        }
        # All receivers acquire new claims under the metadata lock. A dead
        # claim therefore cannot acquire a new owner between probe and unlink.
        unlink($claim) or die "retire released profile claim: $!";
        link($lease_path, $claim) or die "take over profile claim: $!";
    }
    $claims{$key} = $claim;
    push @reply, "claimed $key\n";
}

# Match the legacy store's collection schedule and retention. Live operation
# pins survive eviction of their store names (including by older controllers).
if ((! -f "$store/.collected") || -M _ > 1 / 24) {
    opendir(my $blobs, $store) or die "list profile objects: $!";
    for my $name (readdir($blobs)) {
        next unless $name =~ /\A[0-9a-f]{64}(?:\.x)?\z/;
        my $blob = "$store/$name";
        if (-f $blob && -M _ > 1) {
            unlink($blob) or $!{ENOENT} or die "collect profile object: $!";
        }
    }
    open(my $stamp, '>', "$store/.collected") or die "mark profile store collection: $!";
}
# Reap crashed receivers and expired unowned claim names on the same schedule
# as profile objects. Claim takeover itself depends on the lock, not age.
if ((! -f "$operations/collected") || -M _ > 1 / 24) {
    opendir(my $dirs, $operations) or die "list profile operations: $!";
    for my $name (readdir($dirs)) {
        next unless $name =~ /\Areceive-[a-zA-Z0-9_]+\z/ && "$operations/$name" ne $work;
        my $directory = "$operations/$name";
        my $old;
        if (! -e "$directory/lease") {
            remove_tree($directory, {safe => 1});
        } elsif (open($old, '<', "$directory/lease") && flock($old, LOCK_EX | LOCK_NB)) {
            remove_tree($directory, {safe => 1});
        }
    }
    opendir(my $locks, "$operations/claims") or die "list profile claims: $!";
    for my $name (readdir($locks)) {
        next unless $name =~ /\A[0-9a-f]{64}(?:\.x)?\z/;
        my $path = "$operations/claims/$name";
        next unless -M $path > 1;
        if (!claim_is_live($path)) {
            unlink($path) or $!{ENOENT} or die "collect profile claim: $!";
        }
    }
    open(my $stamp, '>', "$operations/collected") or die "mark profile operation collection: $!";
}
flock($metadata, LOCK_UN) or die "unlock profile store: $!";

$| = 1;
print @reply, "\n";
exit 0 if !%claims && ($busy || !defined($destination));
# The client sends nothing else until this reply, so its next bytes have
# never entered Perl's line buffer and tar can read directly from stdin.
mkdir "$work/incoming" or die "create profile archive staging: $!";
system('tar', '-xzf', '-', '--no-same-owner', '--no-same-permissions', '-C', "$work/incoming") == 0
    or die "extract negotiated profile archive";
for my $key (sort keys %claims) {
    my $blob = "$work/incoming/objects/$key";
    open(my $file, '<:raw', $blob) or die "read claimed profile object: $!";
    die "profile object length mismatch: $key" unless -s $file == $offered{$key};
    my $digest = Digest::SHA->new(256)->addfile($file)->hexdigest;
    die "profile object digest mismatch: $key" unless $digest eq substr($key, 0, 64);
    chmod(0600, $blob) or die "protect profile object: $!";
    # Pin the verified inode before publication. Collection or a compatible
    # older controller can change the store name, never this operation's pin.
    link($blob, "$work/pins/$key") or die "pin received profile object: $!";
    flock($metadata, LOCK_EX) or die "lock profile store: $!";
    rename($blob, "$store/$key") or die "publish profile object: $!";
    unlink($claims{$key}) or die "release published profile claim: $!";
    flock($metadata, LOCK_UN) or die "unlock profile store: $!";
}
exit 0 if $busy || !defined($destination);
open(my $manifest, '<:raw', "$work/incoming/manifest.json") or die "read profile manifest: $!";
my $entries = do { local $/; decode_json(<$manifest>) };
my $tree = "$work/profile";
mkdir $tree or die "create materialized profile: $!";
my @directories;
for my $entry (@$entries) {
    my $path = pack('H*', $entry->{path});
    die "invalid profile path" if $path =~ m{(?:\A/|\0|(?:\A|/)\.\.?(/|\z))};
    my $target = length($path) ? "$tree/$path" : $tree;
    if ($entry->{directory}) {
        mkdir($target) or die "create profile directory: $!" if length($path);
        push @directories, [$target, $entry->{mode}];
    } else {
        my $key = $entry->{key};
        die "unoffered profile object" unless exists $offered{$key};
        die "inconsistent profile entry size" unless $entry->{size} == $offered{$key};
        copy("$work/pins/$key", $target) or die "materialize profile object: $!";
        chmod($entry->{mode}, $target) or die "set materialized profile mode: $!";
    }
}
if (-f "$work/incoming/private.tar") {
    system('tar', '-xf', "$work/incoming/private.tar", '--no-same-owner',
        '--no-same-permissions', '-C', $tree) == 0 or die "extract private profile bundle";
}
for my $directory (reverse @directories) {
    chmod($directory->[1], $directory->[0]) or die "set profile directory mode: $!";
}
# The caller uses a session-specific staging destination and only copies it
# into the home after success. The tree sits beside the store, so publishing
# it is one rename; a destination on another filesystem is copied in pure Perl
# (no cp) into a sibling directory first, so the final rename is still atomic.
$destination =~ s{(?<=.)/+\z}{};
make_path(dirname($destination));
remove_tree($destination);
if (!rename($tree, $destination)) {
    die "stage materialized profile: $!" unless $!{EXDEV};
    my $mode = (stat($tree))[2];
    defined($mode) or die "inspect materialized profile: $!";
    $exdev_staging = tempdir('stage-XXXXXXXXXX', DIR => dirname($destination));
    copy_directory_contents($tree, $exdev_staging);
    chmod($mode & 07777, $exdev_staging) or die "set staged profile mode: $!";
    rename($exdev_staging, $destination) or die "stage materialized profile: $!";
    undef $exdev_staging;
}
