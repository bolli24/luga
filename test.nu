#!/usr/bin/env nu

def main [folder?: string] {
    if ($folder | is-empty) {
        print "Error: Please provide a folder path"
        exit 1
    }
    
    if not ($folder | path exists) {
        print $"Error: Folder '($folder)' does not exist"
        exit 1
    }
    
    if ($folder | path type) != "dir" {
        print $"Error: '($folder)' is not a directory"
        exit 1
    }
    
    let files = (ls $folder | where type == file | get name)
    
    if ($files | is-empty) {
        print "No files found"
        exit 0
    }
    
    for file in $files {
        print $"Running: ($file)"
        cargo run run $file
    }
}
