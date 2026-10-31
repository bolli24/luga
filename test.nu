#!/usr/bin/env nu
def main [folder?: string] {
    
    if ($folder | is-empty) {
        print $"(ansi red)Error: Please provide a folder path(ansi reset)"
        exit 1
    }
    if not ($folder | path exists) {
        print $"(ansi red)Error: Folder '($folder)' does not exist(ansi reset)"
        exit 1
    }
    if ($folder | path type) != "dir" {
        print $"(ansi red)Error: '($folder)' is not a directory(ansi reset)"
        exit 1
    }
    
    let files = (ls $folder | where type == file | get name)
    if ($files | is-empty) {
        print $"(ansi yellow)No files found(ansi reset)"
        exit 0
    }
    
    # Initialize counters for summary
    mut passed = 0
    mut failed = 0
    mut skipped = 0
    mut build_failed = 0
    
    for file in $files {
        let content = (open $file)
        let first_line = ($content | lines | first)
        
        if ($first_line | str starts-with "///") and ($first_line | str contains "SKIP_TEST") {
            print $"(ansi yellow)Skipping: ($file)(ansi reset)"
            $skipped += 1
            continue
        }
        
        let expected_exit_code = if ($first_line | str starts-with "///") and ($first_line | str contains "EXIT_CODE=") {
            let parts = ($first_line | split row "EXIT_CODE=" | get 1 | split row " " | get 0)
            $parts | into int
        } else {
            0
        }
        
        print $"(ansi cyan)Running: ($file)(ansi reset)"
        
        # First build the file
        let build_result = (do { with-env { RUSTFLAGS: "-Awarnings" } { cargo run build $file } } | complete)
        print $build_result.stdout
        print $build_result.stderr
        if $build_result.exit_code != 0 {
            print $"(ansi red)✗ ($file) build failed(ansi reset)"
            $build_failed += 1
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
        print $result.stdout
        print $result.stderr
        if $result.exit_code == $expected_exit_code {
            print $"(ansi green)✓ ($file)(ansi reset)"
            $passed += 1
        } else {
            print $"(ansi red)✗ ($file) expected exit code ($expected_exit_code), got ($result.exit_code)(ansi reset)"
            $failed += 1
        }
    }
    
    # Print summary
    print ""
    print $"(ansi cyan)Test Summary:(ansi reset) (ansi red)($failed)(ansi reset)/(ansi green)($passed)(ansi reset)/(ansi blue)(($passed + $failed + $skipped + $build_failed))(ansi reset) | (ansi yellow)Skipped: ($skipped)(ansi reset) | (ansi red)Build Failed: ($build_failed)(ansi reset)"
    
    # Exit with appropriate code
    if $failed > 0 or $build_failed > 0 {
        exit 1
    }
}
