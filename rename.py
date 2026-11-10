import os
import fnmatch

# ==========================================
# CONFIGURATION
# ==========================================
# Target project folder (Change '.' to your actual project path if needed)
PROJECT_DIR = '.' 

# Words to swap (handles different letter casing automatically)
OLD_NAME = "LuminOS"
NEW_NAME = "LuminOS"

# Optional: Add patterns or folders you want to skip (e.g., git history, binaries)
IGNORE_PATTERNS = ['.git', '.o', '.bin', '.iso', '__pycache__', '.DS_Store']
# ==========================================

def should_ignore(path):
    for pattern in IGNORE_PATTERNS:
        if pattern in path or fnmatch.fnmatch(path, pattern):
            return True
    return False

def replace_in_file_content():
    print("🔄 Step 1: Replacing text inside files...")
    for root, dirs, files in os.walk(PROJECT_DIR, topdown=True):
        # Filter out ignored directories in-place
        dirs[:] = [d for d in dirs if not should_ignore(os.path.join(root, d))]
        
        for file in files:
            file_path = os.path.join(root, file)
            if should_ignore(file_path):
                continue
                
            # Read content
            try:
                with open(file_path, 'r', encoding='utf-8', errors='ignore') as f:
                    content = f.read()
            except Exception:
                continue # Skip files that can't be read as text

            # Check if any casing variation exists
            if (OLD_NAME in content or 
                OLD_NAME.lower() in content or 
                OLD_NAME.upper() in content):
                
                # Perform case-aware replacement
                updated = content.replace(OLD_NAME, NEW_NAME)
                updated = updated.replace(OLD_NAME.lower(), NEW_NAME.lower())
                updated = updated.replace(OLD_NAME.upper(), NEW_NAME.upper())
                
                # Write back changes
                try:
                    with open(file_path, 'w', encoding='utf-8') as f:
                        f.write(updated)
                    print(f"  📝 Updated contents: {file_path}")
                except Exception as e:
                    print(f"  ❌ Failed to write to {file_path}: {e}")

def rename_files_and_dirs():
    print("\n🔄 Step 2: Renaming files and folders...")
    # topdown=False is crucial so we rename children before their parent folders change names
    for root, dirs, files in os.walk(PROJECT_DIR, topdown=False):
        
        # Rename files first
        for file in files:
            if OLD_NAME.lower() in file.lower():
                old_path = os.path.join(root, file)
                if should_ignore(old_path):
                    continue
                
                # Apply case replacements to the filename
                new_file = file.replace(OLD_NAME, NEW_NAME)
                new_file = new_file.replace(OLD_NAME.lower(), NEW_NAME.lower())
                new_file = new_file.replace(OLD_NAME.upper(), NEW_NAME.upper())
                
                new_path = os.path.join(root, new_file)
                os.rename(old_path, new_path)
                print(f"  📁 Renamed File: {old_path} -> {new_file}")

        # Rename directories next
        for d in dirs:
            if OLD_NAME.lower() in d.lower():
                old_path = os.path.join(root, d)
                if should_ignore(old_path):
                    continue
                
                new_dir = d.replace(OLD_NAME, NEW_NAME)
                new_dir = new_dir.replace(OLD_NAME.lower(), NEW_NAME.lower())
                new_dir = new_dir.replace(OLD_NAME.upper(), NEW_NAME.upper())
                
                new_path = os.path.join(root, new_dir)
                os.rename(old_path, new_path)
                print(f"  📂 Renamed Folder: {old_path} -> {new_dir}")

if __name__ == "__main__":
    # Safety warning
    confirm = input("⚠️ This will modify files permanently. Back up your project first! Type 'yes' to proceed: ")
    if confirm.lower() == 'yes':
        replace_in_file_content()
        rename_files_and_dirs()
        print("\n🎉 Refactoring complete! LuminOS is officially live.")
    else:
        print("❌ Aborted.")
