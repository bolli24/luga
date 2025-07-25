#!/usr/bin/env nu

def main [folder?: string] {
    let red = "\e[31m"
    let green = "\e[32m"
    let yellow = "\e[33m"
    let blue = "\e[34m"
    let cyan = "\e[36m"
    let reset = "\e[0m"

    if ($folder | is-empty) {
        print $"($red)Error: Please provide a folder path($reset)"
        exit 1
    }

    if not ($folder | path exists) {
        print $"($red)Error: Folder '($folder)' does not exist($reset)"
        exit 1
    }

    if ($folder | path type) != "dir" {
        print $"($red)Error: '($folder)' is not a directory($reset)"
        exit 1
    }

    let files = (ls $folder | where type == file | get name)

    if ($files | is-empty) {
        print $"($yellow)No files found($reset)"
        exit 0
    }

    for file in $files {
        let content = (open $file)
        let first_line = ($content | lines | first)
        if ($first_line | str starts-with "///") and ($first_line | str contains "SKIP_TEST") {
            print $"($yellow)Skipping: ($file)($reset)"
            continue
        }

        let expected_exit_code = if ($first_line | str starts-with "///") and ($first_line | str contains "EXIT_CODE=") {
            let parts = ($first_line | split row "EXIT_CODE=" | get 1 | split row " " | get 0)
            $parts | into int
        } else {
            0
        }

        print $"($cyan)Running: ($file)($reset)"

        # First build the file
        let build_result = (do { cargo run build $file } | complete)



        if $build_result.exit_code != 0 {
            print $"($red)✗ ($file) build failed($reset)"
            print $build_result.stdout
            print $build_result.stderr
            continue
        }

        # Get the executable name (with .exe on Windows)
        let exe_name = if $nu.os-info.name == "windows" {
            ($file | path parse | get stem) + ".exe"
        } else {
            ($file | path parse | get stem)
        }

        let exe_path = $"output/debug/($exe_name)"

        # Execute the built file
        let result = (do { ^$exe_path } | complete)

        if $result.exit_code == $expected_exit_code {
            print $"($green)✓ ($file)($reset)"
        } else {
            print $"($red)✗ ($file) expected exit code ($expected_exit_code), got ($result.exit_code)($reset)"
        }
    }
}
