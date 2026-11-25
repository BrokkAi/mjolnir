// Port of mj-cli/src/image_commands.rs: the arguments of `mj image`. The
// command itself (building, publishing and comparing recipe images) is not
// ported yet.

package cli

import "github.com/spf13/cobra"

// imageArgs are the arguments of `mj image`.
type imageArgs struct {
	command imageCommand
}

// imageCommand is a `mj image` subcommand.
type imageCommand interface{ isImageCommand() }

// imageCommandBuild builds, pushes, and assigns an image from the
// repository's mise recipe.
type imageCommandBuild struct {
	// path is a directory inside the repository; defaults to the current
	// directory.
	path *string
	// bundle is the bundle to assign; defaults to the session's bundle or
	// this local repository.
	bundle *string
	// target is the container target whose configured image supplies the
	// base.
	target *string
	// name is the registry image name; defaults to the bundle id.
	name *string
	// print prints the deterministic Containerfile without building or
	// publishing.
	print bool
}

// imageCommandStatus compares published project images with the
// repository's current recipe.
type imageCommandStatus struct {
	// path is a directory inside the repository; defaults to the current
	// directory.
	path *string
	// bundle is the bundle to inspect; defaults to the session's bundle or
	// this local repository.
	bundle *string
	// target is the target whose base image determines the expected recipe
	// hash.
	target *string
}

func (imageCommandBuild) isImageCommand()  {}
func (imageCommandStatus) isImageCommand() {}

// bind declares the image subcommands on cmd; run runs the parsed command.
func (args *imageArgs) bind(cmd *cobra.Command, run func() error) {
	meta := clapOf(cmd)

	var build imageCommandBuild
	buildCmd := subcommand("build", "Build, push, and assign an image from the repository's mise recipe")
	meta.subcommand(buildCmd)
	buildClap := clapOf(buildCmd)
	buildClap.option("path", "Directory inside the repository; defaults to the current directory", storeOptional(&build.path))
	buildClap.option("bundle", "Bundle to assign; defaults to the session's bundle or this local repository", storeOptional(&build.bundle))
	buildClap.option("target", "Container target whose configured image supplies the base", storeOptional(&build.target))
	buildClap.option("name", "Registry image name; defaults to the bundle id", storeOptional(&build.name))
	buildClap.flag(&build.print, "print", "Print the deterministic Containerfile without building or publishing")
	leaf(buildCmd, func() error {
		args.command = build
		return run()
	})

	var status imageCommandStatus
	statusCmd := subcommand("status", "Compare published project images with the repository's current recipe")
	meta.subcommand(statusCmd)
	statusClap := clapOf(statusCmd)
	statusClap.option("path", "Directory inside the repository; defaults to the current directory", storeOptional(&status.path))
	statusClap.option("bundle", "Bundle to inspect; defaults to the session's bundle or this local repository", storeOptional(&status.bundle))
	statusClap.option("target", "Target whose base image determines the expected recipe hash", storeOptional(&status.target))
	leaf(statusCmd, func() error {
		args.command = status
		return run()
	})
	group(cmd, true, nil)
}
