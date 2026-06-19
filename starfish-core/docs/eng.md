# 1. Make sure rust-analyzer is available
rustup component add rust-analyzer

# 2. Install the MCP server
cargo install rust-analyzer-mcp

# 3. Add to Claude Code (project scope — good for per-repo use)
claude mcp add --scope project rust-analyzer -- rust-analyzer-mcp

# OR user scope (available in all your projects)
claude mcp add --scope user rust-analyzer -- rust-analyzer-mcp
