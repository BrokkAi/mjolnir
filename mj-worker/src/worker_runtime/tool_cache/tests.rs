use super::*;
use std::time::Duration;

fn environment(root: &Path, checkout: &Path) -> BTreeMap<String, String> {
    let mut env = ["PATH", "JAVA_HOME", "LANG", "SSL_CERT_FILE", "SSL_CERT_DIR"]
        .into_iter()
        .filter_map(|name| std::env::var(name).ok().map(|value| (name.into(), value)))
        .collect::<BTreeMap<_, _>>();
    for (name, value) in [
        ("HOME", root.join("home")),
        ("MJ_TOOL_CACHE_DIR", root.join("shared cache $literal")),
        (
            "MJ_TOOL_CACHE_HOME",
            root.join("workers").join(checkout.file_name().unwrap()),
        ),
        ("MJ_NX_HOME_DIR", root.join("home/.nx")),
        ("XDG_CONFIG_HOME", root.join("home/.config")),
        ("MBX_CACHE_DIR", root.join("mbx")),
        (
            "MBX_SHIMS_DIR",
            root.join("shims").join(checkout.file_name().unwrap()),
        ),
    ] {
        env.insert(name.into(), value.to_string_lossy().into_owned());
    }
    env.insert(
        "MJ_TOOL_CACHE_PROJECT".into(),
        "cache-fixture.example/project".into(),
    );
    env.insert("NX_DAEMON".into(), "false".into());
    env.insert("NX_NO_CLOUD".into(), "true".into());
    env.insert("DO_NOT_TRACK".into(), "1".into());
    env.insert("TURBO_TELEMETRY_DISABLED".into(), "1".into());
    env.insert("MBX_SUMMARY".into(), "full".into());
    env.insert("MBX_SAVINGS".into(), "off".into());
    env.insert("GOTOOLCHAIN".into(), "local".into());
    // Both checkouts represent one revision. Separate container invocations
    // can cross a second boundary; Git's default clock would change Go's VCS
    // build metadata and correctly force a rebuild.
    for key in ["GIT_AUTHOR_DATE", "GIT_COMMITTER_DATE"] {
        env.insert(key.into(), "2026-01-01T00:00:00Z".into());
    }
    env
}

async fn run(
    cwd: &Path,
    env: &BTreeMap<String, String>,
    command: &str,
    args: &[&str],
) -> Result<String> {
    // Resolve like a shell; Command's own PATH search can otherwise consult
    // the test process PATH instead of the child's explicit environment.
    let executable = super::super::harness_launch::find_command(Path::new(command), env)?
        .with_context(|| format!("test tool {command} is missing"))?;
    let mut child = tokio::process::Command::new(executable);
    child.current_dir(cwd).env_clear().envs(env).args(args);
    let container = if let Ok(image) = std::env::var("MJ_CACHE_TEST_PODMAN_IMAGE") {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let name = format!(
            "mj-cache-test-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        );
        let root = Path::new(&env["HOME"])
            .parent()
            .context("fixture home has no parent")?;
        let modules = PathBuf::from(std::env::var("MJ_CACHE_TEST_NODE_MODULES")?);
        let tools = modules.parent().context("fixture modules have no parent")?;
        let executable = child.as_std().get_program().to_owned();
        child = tokio::process::Command::new("podman");
        child.args([
            "run",
            "--rm",
            "--pull=never",
            "--timeout=175",
            "--user=0:0",
            "--label=dev.mj.instance=tool-cache-test",
            "--name",
            &name,
        ]);
        for (source, access) in [(root, "rw"), (tools, "ro")] {
            child.args([
                "--volume",
                &format!("{}:{}:{access}", source.display(), source.display()),
            ]);
        }
        child.arg("--workdir").arg(cwd);
        for (name, value) in env {
            child.arg("--env").arg(format!("{name}={value}"));
        }
        child.arg(image).arg(executable).args(args);
        Some(name)
    } else {
        None
    };
    let output =
        mj_core::subprocess::run_bounded(&mut child, 4 * 1024 * 1024, Duration::from_secs(180))
            .await;
    if let Some(name) = container {
        // Stop the container before any error can drop the fixture directory.
        let mut cleanup = tokio::process::Command::new("podman");
        cleanup.args(["rm", "--ignore", "--force", &name]);
        let cleaned =
            mj_core::subprocess::run_bounded(&mut cleanup, 64 * 1024, Duration::from_secs(30))
                .await?;
        ensure!(
            cleaned.status.success(),
            "could not stop fixture container {name}"
        );
    }
    let output = output?;
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    ensure!(
        output.status.success(),
        "{command} {args:?} failed in {}:\n{text}",
        cwd.display()
    );
    Ok(text)
}

#[tokio::test]
async fn generated_launchers_preserve_shell_arguments_and_explicit_bazel_flags() -> Result<()> {
    let root = tempfile::tempdir()?;
    let checkout = root.path().join("checkout ' $name");
    let tools = root.path().join("original tools");
    std::fs::create_dir_all(&checkout)?;
    std::fs::create_dir_all(&tools)?;
    std::fs::write(checkout.join("MODULE.bazel"), "")?;
    write_launcher(&tools.join("bazel"), "printf '<%s>\\n' \"$@\"\n")?;
    let mut env = environment(root.path(), &checkout);
    env.insert("PATH".into(), tools.to_string_lossy().into_owned());
    prepare(&checkout, &mut env)?;
    // Re-preparation must not resolve its own launcher and recurse forever.
    prepare(&checkout, &mut env)?;
    let output = run(
        &checkout,
        &env,
        "bazel",
        &[
            "--batch",
            "build",
            "--disk_cache=/explicit user's cache",
            "//:target $literal",
        ],
    )
    .await?;
    let lines = output.lines().collect::<Vec<_>>();
    ensure!(lines[0].starts_with("<--bazelrc="));
    ensure!(
        lines[1..]
            == [
                "<--batch>",
                "<build>",
                "<--disk_cache=/explicit user's cache>",
                "<//:target $literal>"
            ]
    );
    ensure!(run(&checkout, &env, "bazel", &["--version"]).await? == "<--version>\n");
    // Git setup and tool setup must replace the one owned shell hook, never
    // source each other in a cycle when the worker is prepared again.
    let user_hook = root.path().join("user bash env");
    std::fs::write(
        &user_hook,
        "if [ -n \"${USER_HOOK_SEEN:-}\" ]; then exit 99; fi\nexport USER_HOOK_SEEN=yes\n",
    )?;
    env.insert("BASH_ENV".into(), user_hook.to_string_lossy().into_owned());
    let git_hook = root.path().join("github-shell-env");
    super::super::shell_environment::configure(&git_hook, &mut env)?;
    prepare(&checkout, &mut env)?;
    super::super::shell_environment::configure(&git_hook, &mut env)?;
    prepare(&checkout, &mut env)?;
    env.insert("PATH".into(), "/usr/bin:/bin".into());
    let output = run(
        &checkout,
        &env,
        "bash",
        &["-c", "printf '%s\\n' \"$USER_HOOK_SEEN\"; bazel --version"],
    )
    .await?;
    ensure!(output == "yes\n<--version>\n", "{output}");
    // Reviewers can prepare another checkout from the primary environment.
    // Its launcher must resolve the actual tool, not wrap the first checkout.
    let other = root.path().join("review checkout");
    std::fs::create_dir_all(&other)?;
    std::fs::write(other.join("MODULE.bazel"), "")?;
    env.insert(
        "PATH".into(),
        format!("{}:{}", env["MJ_BUILD_TOOL_BIN"], tools.display()),
    );
    prepare(&other, &mut env)?;
    let output = run(&other, &env, "bazel", &["build", "//:target"]).await?;
    ensure!(output.matches("<--bazelrc=").count() == 1, "{output}");
    Ok(())
}

/// Integration with actual tool formats and cache keys. The same portable test
/// executable runs on a raw host and in Podman. Build artifacts stay in the
/// explicitly supplied local-disk root, never in a temporary Cargo target.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires Go, Gradle/JDK, Nx 23.2+, Turbo, Bazelisk, CMake, make, mbx and MJ_CACHE_TEST_ROOT/MJ_CACHE_TEST_NODE_MODULES"]
async fn real_tools_reuse_invalidate_and_build_concurrently() -> Result<()> {
    let base = PathBuf::from(
        std::env::var("MJ_CACHE_TEST_ROOT")
            .context("set MJ_CACHE_TEST_ROOT to disposable local storage")?,
    );
    std::fs::create_dir_all(&base)?;
    let root = tempfile::Builder::new()
        .prefix("build-caches-")
        .tempdir_in(base)?;
    let modules = PathBuf::from(std::env::var("MJ_CACHE_TEST_NODE_MODULES")?);
    let a = root.path().join("checkout A");
    let b = root.path().join("checkout B");
    for checkout in [&a, &b] {
        fixture(checkout, &modules)?;
    }
    std::fs::create_dir_all(root.path().join("home/.nx"))?;
    let mut ea = environment(root.path(), &a);
    let mut eb = environment(root.path(), &b);
    for (checkout, env) in [(&a, &mut ea), (&b, &mut eb)] {
        run(checkout, env, "git", &["init", "-q"]).await?;
        run(
            checkout,
            env,
            "git",
            &[
                "remote",
                "add",
                "origin",
                "https://cache-fixture.example/project.git",
            ],
        )
        .await?;
        run(checkout, env, "git", &["add", "."]).await?;
        run(
            checkout,
            env,
            "git",
            &[
                "-c",
                "user.name=Cache Test",
                "-c",
                "user.email=cache@example.test",
                "commit",
                "-qm",
                "fixture",
            ],
        )
        .await?;
        prepare(checkout, env)?;
    }
    for (name, args) in [
        ("go", vec!["version"]),
        ("gradle", vec!["--version"]),
        ("nx", vec!["--version"]),
        ("turbo", vec!["--version"]),
        ("bazelisk", vec!["--batch", "version"]),
        ("mbx", vec!["--version"]),
    ] {
        println!("{name}: {}", run(&a, &ea, name, &args).await?.trim());
    }

    let go = ["build", "-x", "-o", "app", "."];
    run(&a, &ea, "go", &go).await?;
    let cached = run(&b, &eb, "go", &go).await?;
    ensure!(
        !cached.contains("/compile -o"),
        "Go recompiled across checkouts:\n{cached}"
    );
    ensure!(
        run(&b, &eb, b.join("app").to_str().unwrap(), &[])
            .await?
            .trim()
            == "one"
    );
    std::fs::write(b.join("value.go"), "package main\nconst value = \"two\"\n")?;
    run(&b, &eb, "go", &go).await?;
    ensure!(
        run(&b, &eb, b.join("app").to_str().unwrap(), &[])
            .await?
            .trim()
            == "two"
    );
    println!("Go: second checkout reused compiled packages; input change rebuilt correctly");

    let gradle = [
        "--no-daemon",
        "--max-workers=2",
        "--console=plain",
        "compileJava",
    ];
    run(&a, &ea, "gradle", &gradle).await?;
    let cached = run(&b, &eb, "gradle", &gradle).await?;
    ensure!(
        cached.contains(":compileJava FROM-CACHE"),
        "Gradle missed:\n{cached}"
    );
    let original = std::fs::read(b.join("build/classes/java/main/Value.class"))?;
    std::fs::write(
        b.join("src/main/java/Value.java"),
        "public class Value { public static final String VALUE = \"two\"; }\n",
    )?;
    run(&b, &eb, "gradle", &gradle).await?;
    ensure!(std::fs::read(b.join("build/classes/java/main/Value.class"))? != original);
    println!("Gradle: FROM-CACHE across private user homes; input change rebuilt correctly");

    for (tool, args, output) in [
        ("turbo", vec!["run", "build"], "turbo"),
        (
            "nx",
            vec!["run", "fixture:build", "--outputStyle=static"],
            "nx",
        ),
    ] {
        run(&a, &ea, tool, &args).await?;
        let cached = run(&b, &eb, tool, &args).await?;
        ensure!(
            !b.join(format!("{output}-calls.txt")).exists(),
            "{tool} executed in second checkout:\n{cached}"
        );
        ensure!(std::fs::read_to_string(b.join(format!("out/{output}/value.txt")))? == "one\n");
        std::fs::write(b.join("input.txt"), "two\n")?;
        run(&b, &eb, tool, &args).await?;
        ensure!(std::fs::read_to_string(b.join(format!("out/{output}/value.txt")))? == "two\n");
        std::fs::write(b.join("input.txt"), "one\n")?;
        println!("{tool}: restored outputs without executing; input change rebuilt correctly");
    }

    let bazel = [
        "--batch",
        "build",
        "--enable_bzlmod=false",
        "--enable_workspace=true",
        "//:copy",
    ];
    run(&a, &ea, "bazelisk", &bazel).await?;
    let cached = run(&b, &eb, "bazelisk", &bazel).await?;
    ensure!(cached.contains("disk cache hit"), "Bazel missed:\n{cached}");
    std::fs::write(b.join("input.txt"), "two\n")?;
    run(&b, &eb, "bazelisk", &bazel).await?;
    ensure!(std::fs::read_to_string(b.join("bazel-bin/copied.txt"))? == "two\n");
    println!("Bazel: disk cache hit across output bases; input change rebuilt correctly");

    for (checkout, env) in [(&a, &ea), (&b, &eb)] {
        run(
            checkout,
            env,
            "cmake",
            &[
                "-S",
                ".",
                "-B",
                "cmake-build",
                "-DCMAKE_C_COMPILER=/usr/bin/cc",
            ],
        )
        .await?;
        let result = run(
            checkout,
            env,
            "cmake",
            &["--build", "cmake-build", "--", "-j2"],
        )
        .await?;
        println!("CMake {}: {result}", checkout.display());
        if checkout == &b {
            ensure!(
                result.contains("1 hits, 0 misses"),
                "C/C++ missed:\n{result}"
            );
        }
        ensure!(
            run(
                checkout,
                env,
                checkout.join("cmake-build/native").to_str().unwrap(),
                &[]
            )
            .await?
            .trim()
                == "one"
        );
    }
    // Shared in-flight writes must preserve each checkout's own outputs.
    for checkout in [&a, &b] {
        std::fs::write(
            checkout.join("value.go"),
            "package main\nconst value = \"concurrent\"\n",
        )?;
        std::fs::write(checkout.join("input.txt"), "concurrent\n")?;
        std::fs::write(
            checkout.join("src/main/java/Value.java"),
            "public class Value { public static final String VALUE = \"concurrent\"; }\n",
        )?;
        std::fs::write(
            checkout.join("native/native.c"),
            "#include <stdio.h>\nint main(void) { puts(\"concurrent\"); }\n",
        )?;
    }
    for (tool, args) in [
        ("go", go.to_vec()),
        ("gradle", gradle.to_vec()),
        ("turbo", vec!["run", "build"]),
        ("nx", vec!["run", "fixture:build", "--outputStyle=static"]),
        ("bazelisk", bazel.to_vec()),
        ("cmake", vec!["--build", "cmake-build", "--", "-j2"]),
    ] {
        tokio::try_join!(run(&a, &ea, tool, &args), run(&b, &eb, tool, &args))?;
        println!("{tool}: concurrent independent builds succeeded");
    }
    for (checkout, env) in [(&a, &ea), (&b, &eb)] {
        for file in [
            "out/turbo/value.txt",
            "out/nx/value.txt",
            "bazel-bin/copied.txt",
        ] {
            ensure!(
                std::fs::read_to_string(checkout.join(file))? == "concurrent\n",
                "{} {file}",
                checkout.display()
            );
        }
        for file in ["app", "cmake-build/native"] {
            ensure!(
                run(checkout, env, checkout.join(file).to_str().unwrap(), &[])
                    .await?
                    .trim()
                    == "concurrent"
            );
        }
    }
    println!("Evidence retained in {}", root.keep().display());
    Ok(())
}

fn fixture(root: &Path, modules: &Path) -> Result<()> {
    std::fs::create_dir_all(root.join("src/main/java"))?;
    std::fs::create_dir_all(root.join("native"))?;
    for (name, content) in [
        (
            ".gitignore",
            "node_modules\nout\n*-calls.txt\n.nx\n.turbo\n.gradle\nbuild\nbazel-*\ncmake-build\napp\n",
        ),
        ("go.mod", "module cachetest.example/check\n\ngo 1.23\n"),
        (
            "main.go",
            "package main\nimport \"fmt\"\nfunc main() { fmt.Println(value) }\n",
        ),
        ("value.go", "package main\nconst value = \"one\"\n"),
        ("settings.gradle", "rootProject.name = 'cache-fixture'\n"),
        ("build.gradle", "plugins { id 'java' }\n"),
        (
            "src/main/java/Value.java",
            "public class Value { public static final String VALUE = \"one\"; }\n",
        ),
        ("input.txt", "one\n"),
        (
            "package.json",
            r#"{"name":"fixture","version":"1.0.0","private":true,"packageManager":"npm@10.8.2","scripts":{"build":"node build.cjs web"},"devDependencies":{"nx":"23.3.0","turbo":"2.11.7"}}"#,
        ),
        (
            "package-lock.json",
            r#"{"name":"fixture","version":"1.0.0","lockfileVersion":3,"packages":{"":{"name":"fixture","version":"1.0.0"}}}"#,
        ),
        (
            "turbo.json",
            r#"{"tasks":{"build":{"inputs":["input.txt","build.cjs"],"outputs":["out/turbo/**"]}}}"#,
        ),
        (
            "nx.json",
            r#"{"defaultBase":"master","targetDefaults":{"build":{"cache":true}}}"#,
        ),
        (
            "project.json",
            r#"{"name":"fixture","targets":{"build":{"executor":"nx:run-commands","cache":true,"inputs":["{workspaceRoot}/input.txt","{workspaceRoot}/build.cjs"],"outputs":["{workspaceRoot}/out/nx"],"options":{"command":"node build.cjs nx"}}}}"#,
        ),
        (
            "build.cjs",
            "const fs=require('fs'); const name=process.argv[2]==='web'?'turbo':process.argv[2]; fs.mkdirSync('out/'+name,{recursive:true}); fs.copyFileSync('input.txt','out/'+name+'/value.txt'); fs.appendFileSync(name+'-calls.txt','executed\\n');\n",
        ),
        (".bazelversion", "8.3.1\n"),
        ("WORKSPACE", "workspace(name = \"cache_fixture\")\n"),
        (
            "BUILD",
            "genrule(name = \"copy\", srcs = [\"input.txt\"], outs = [\"copied.txt\"], cmd = \"cp $(location input.txt) $@\")\n",
        ),
        (
            "CMakeLists.txt",
            "cmake_minimum_required(VERSION 3.16)\nproject(CacheFixture C)\nadd_executable(native native/native.c)\n",
        ),
        (
            "native/native.c",
            "#include <stdio.h>\nint main(void) { puts(\"one\"); }\n",
        ),
    ] {
        std::fs::write(root.join(name), content)?;
    }
    std::os::unix::fs::symlink(modules, root.join("node_modules"))?;
    Ok(())
}
