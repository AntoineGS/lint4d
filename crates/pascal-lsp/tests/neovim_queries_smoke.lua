local REQUEST_TIMEOUT = 10000

local function assert_range(actual, start_line, start_character, end_line, end_character)
	local expected = {
		start = { line = start_line, character = start_character },
		["end"] = { line = end_line, character = end_character },
	}
	assert(vim.deep_equal(actual, expected), vim.inspect(actual))
end

local function request(client, method, params, bufnr)
	local completed = false
	local callback_error
	local result
	local accepted = client:request(method, params, function(err, response)
		callback_error = err
		result = response
		completed = true
	end, bufnr)
	assert(accepted, method .. " request was not accepted")
	assert(
		vim.wait(REQUEST_TIMEOUT, function()
			return completed
		end),
		method .. " request timed out"
	)
	assert(not callback_error, method .. " callback error: " .. vim.inspect(callback_error))
	assert(result ~= nil, method .. " returned no result")
	return result
end

local function standard_list(label, invoke)
	local captured
	invoke(function(list)
		captured = list
	end)
	assert(
		vim.wait(REQUEST_TIMEOUT, function()
			return captured ~= nil
		end),
		label .. " timed out"
	)
	assert(captured.items and #captured.items > 0, label .. " returned no items")
	return captured
end

local function names(symbols)
	local result = {}
	for _, symbol in ipairs(symbols) do
		result[#result + 1] = symbol.name
	end
	return result
end

local function assert_location(location, expected_uri, start_line, start_character, end_line, end_character)
	assert(location.uri == expected_uri, vim.inspect(location))
	assert_range(location.range, start_line, start_character, end_line, end_character)
end

local function run()
	vim.cmd("filetype on")
	vim.opt.hidden = true
	dofile(assert(vim.env.PASCAL_LSP_CONFIG))

	local root = assert(vim.env.PASCAL_LSP_SMOKE_ROOT)
	local provider_path = root .. "/Provider.pas"
	local consumer_path = root .. "/Consumer.pas"
	local provider_on_disk = vim.fn.readfile(provider_path)
	local consumer_on_disk = vim.fn.readfile(consumer_path)
	local provider_uri = vim.uri_from_fname(provider_path)
	local consumer_uri = vim.uri_from_fname(consumer_path)

	vim.cmd.edit(vim.fn.fnameescape(provider_path))
	local provider = vim.api.nvim_get_current_buf()
	assert(
		vim.wait(REQUEST_TIMEOUT, function()
			return #vim.lsp.get_clients({ bufnr = provider, name = "pascal_lsp" }) == 1
		end),
		"LSP did not attach using the shipped example"
	)
	local client = vim.lsp.get_clients({ bufnr = provider, name = "pascal_lsp" })[1]
	assert(client.offset_encoding == "utf-16", "wrong position encoding")
	assert(vim.fn.bufnr(consumer_path) == -1, "consumer must start unopened")

	local overlay = {
		"unit Provider;",
		"interface",
		"type",
		"  TWidget = class",
		"  private",
		"    FValue: Integer;",
		"  public",
		"    procedure Run;",
		"    property Value: Integer read FValue;",
		"  end;",
		"const",
		"  SharedValue = 1;",
		"  UnsavedOnly = 3;",
		"procedure PublicRoutine;",
		"implementation",
		"procedure TWidget.Run;",
		"begin",
		"  FValue := 1;",
		"end;",
		"procedure PublicRoutine;",
		"begin",
		"  Log(SharedValue);",
		"end;",
		"end.",
	}
	vim.api.nvim_buf_set_lines(provider, 0, -1, false, overlay)
	assert(vim.bo[provider].modified, "overlay must remain unsaved")

	local outline = request(client, "textDocument/documentSymbol", {
		textDocument = { uri = provider_uri },
	}, provider)
	assert(#outline == 1, vim.inspect(outline))
	assert(outline[1].name == "Provider", vim.inspect(outline[1]))
	assert_range(outline[1].range, 0, 0, 24, 0)
	assert_range(outline[1].selectionRange, 0, 5, 0, 13)
	local outline_children = assert(outline[1].children, "hierarchical outline was not returned")
	assert(
		vim.deep_equal(
			names(outline_children),
			{ "TWidget", "SharedValue", "UnsavedOnly", "PublicRoutine", "Run", "PublicRoutine" }
		),
		vim.inspect(names(outline_children))
	)
	local widget = outline_children[1]
	assert(widget.name == "TWidget", vim.inspect(widget))
	assert_range(widget.range, 3, 2, 9, 6)
	assert_range(widget.selectionRange, 3, 2, 3, 9)
	assert(
		vim.deep_equal(names(assert(widget.children, "class members missing")), { "FValue", "Run", "Value" }),
		vim.inspect(widget.children)
	)

	local outline_list = standard_list("gO/document_symbol", function(on_list)
		vim.lsp.buf.document_symbol({ on_list = on_list })
	end)
	local outline_item
	for _, item in ipairs(outline_list.items) do
		if item.text:find("TWidget", 1, true) then
			outline_item = item
		end
	end
	assert(outline_item, "gO/document_symbol did not list TWidget")
	assert(outline_item.lnum == 4 and outline_item.col == 3, vim.inspect(outline_item))

	local workspace_symbols = request(client, "workspace/symbol", { query = "Only" }, provider)
	assert(#workspace_symbols == 3, vim.inspect(workspace_symbols))
	assert(workspace_symbols[1].name == "ConsumerOnly", vim.inspect(workspace_symbols))
	assert(workspace_symbols[1].containerName == "Consumer", vim.inspect(workspace_symbols[1]))
	assert_location(workspace_symbols[1].location, consumer_uri, 3, 0, 3, 23)
	assert(workspace_symbols[2].name == "ConsumerOnly", vim.inspect(workspace_symbols))
	assert_location(workspace_symbols[2].location, consumer_uri, 5, 0, 9, 4)
	assert(workspace_symbols[3].name == "UnsavedOnly", vim.inspect(workspace_symbols))
	assert(workspace_symbols[3].containerName == "Provider", vim.inspect(workspace_symbols[3]))
	assert_location(workspace_symbols[3].location, provider_uri, 12, 2, 12, 18)
	assert(vim.fn.bufnr(consumer_path) == -1, "workspace symbol search opened the consumer")

	local workspace_list = standard_list("workspace_symbol", function(on_list)
		vim.lsp.buf.workspace_symbol("Only", { on_list = on_list })
	end)
	assert(#workspace_list.items == 3, vim.inspect(workspace_list.items))
	assert(vim.fn.bufnr(consumer_path) == -1, "workspace symbol picker opened the consumer")

	local shared_position = { line = 11, character = 2 }
	local references_params = {
		textDocument = { uri = provider_uri },
		position = shared_position,
	}
	local references_without_declaration = request(client, "textDocument/references", {
		textDocument = references_params.textDocument,
		position = references_params.position,
		context = { includeDeclaration = false },
	}, provider)
	assert(#references_without_declaration == 3, vim.inspect(references_without_declaration))
	assert_location(references_without_declaration[1], consumer_uri, 7, 6, 7, 17)
	assert_location(references_without_declaration[2], consumer_uri, 8, 15, 8, 26)
	assert_location(references_without_declaration[3], provider_uri, 21, 6, 21, 17)

	local references_with_declaration = request(client, "textDocument/references", {
		textDocument = references_params.textDocument,
		position = references_params.position,
		context = { includeDeclaration = true },
	}, provider)
	assert(#references_with_declaration == 4, vim.inspect(references_with_declaration))
	assert_location(references_with_declaration[1], consumer_uri, 7, 6, 7, 17)
	assert_location(references_with_declaration[2], consumer_uri, 8, 15, 8, 26)
	assert_location(references_with_declaration[3], provider_uri, 11, 2, 11, 13)
	assert_location(references_with_declaration[4], provider_uri, 21, 6, 21, 17)

	vim.api.nvim_win_set_cursor(0, { 12, 2 })
	local references_list = standard_list("grr/references", function(on_list)
		vim.lsp.buf.references({ includeDeclaration = false }, { on_list = on_list })
	end)
	assert(#references_list.items == 3, vim.inspect(references_list.items))

	local highlights = request(client, "textDocument/documentHighlight", {
		textDocument = { uri = provider_uri },
		position = shared_position,
	}, provider)
	assert(#highlights == 2, vim.inspect(highlights))
	for _, highlight in ipairs(highlights) do
		assert(highlight.kind == nil, vim.inspect(highlight))
		assert(highlight.uri == nil, vim.inspect(highlight))
	end
	assert_range(highlights[1].range, 11, 2, 11, 13)
	assert_range(highlights[2].range, 21, 6, 21, 17)

	assert(vim.deep_equal(vim.fn.readfile(provider_path), provider_on_disk), "server changed Provider.pas on disk")
	assert(vim.deep_equal(vim.fn.readfile(consumer_path), consumer_on_disk), "server changed Consumer.pas on disk")
	assert(vim.bo[provider].modified, "query requests must not save the overlay")
	client:stop()
	assert(
		vim.wait(REQUEST_TIMEOUT, function()
			return client:is_stopped()
		end),
		"server did not shut down"
	)
	io.stdout:write("NEOVIM_QUERY_SMOKE_OK\n")
end

local ok, error_message = xpcall(run, debug.traceback)
if not ok then
	io.stderr:write(tostring(error_message) .. "\n")
	vim.cmd("cquit 1")
else
	vim.cmd("qa!")
end
