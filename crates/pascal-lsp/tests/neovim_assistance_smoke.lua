local REQUEST_TIMEOUT = 10000
local BUSY_ERROR_CODE = -32803
local BUSY_RETRY_DELAY = 50
local MAX_BUSY_RETRIES = REQUEST_TIMEOUT / BUSY_RETRY_DELAY

local function request(client, method, params, bufnr)
	for _ = 1, MAX_BUSY_RETRIES do
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
		if callback_error and callback_error.code == BUSY_ERROR_CODE then
			vim.wait(BUSY_RETRY_DELAY)
		else
			assert(not callback_error, method .. " callback error: " .. vim.inspect(callback_error))
			assert(result ~= nil, method .. " returned no result")
			return result
		end
	end
	error(method .. " remained busy for " .. REQUEST_TIMEOUT .. "ms")
end

local function assert_range(actual, start_line, start_character, end_line, end_character)
	local expected = {
		start = { line = start_line, character = start_character },
		["end"] = { line = end_line, character = end_character },
	}
	assert(vim.deep_equal(actual, expected), vim.inspect(actual))
end

local function item_labels(items)
	local labels = {}
	for _, item in ipairs(items) do
		labels[#labels + 1] = item.label
	end
	return labels
end

local function run()
	vim.cmd("filetype on")
	vim.opt.hidden = true
	dofile(assert(vim.env.PASCAL_LSP_CONFIG))

	local root = assert(vim.env.PASCAL_LSP_SMOKE_ROOT)
	local main_path = root .. "/Main.pas"
	local provider_path = root .. "/Provider.pas"
	local main_on_disk = vim.fn.readfile(main_path)
	local provider_on_disk = vim.fn.readfile(provider_path)

	vim.cmd.edit(vim.fn.fnameescape(main_path))
	local main = vim.api.nvim_get_current_buf()
	assert(
		vim.wait(REQUEST_TIMEOUT, function()
			return #vim.lsp.get_clients({ bufnr = main, name = "pascal_lsp" }) == 1
		end),
		"LSP did not attach using the shipped example"
	)
	local client = vim.lsp.get_clients({ bufnr = main, name = "pascal_lsp" })[1]
	assert(client.offset_encoding == "utf-16", "wrong position encoding")

	local overlay = vim.api.nvim_buf_get_lines(main, 0, -1, false)
	overlay[17] = "  OverlayName: Integer;"
	overlay[19] = "  Over;"
	vim.api.nvim_buf_set_lines(main, 0, -1, false, overlay)
	assert(vim.bo[main].modified, "assistance fixture must remain unsaved")

	local main_uri = vim.uri_from_fname(main_path)
	local overlay_hover = request(client, "textDocument/hover", {
		textDocument = { uri = main_uri },
		position = { line = 16, character = 5 },
	}, main)
	assert(overlay_hover.range, "hover did not return an identifier range")
	assert_range(overlay_hover.range, 16, 2, 16, 13)
	assert(
		overlay_hover.contents.kind == "plaintext" or overlay_hover.contents.kind == "markdown",
		vim.inspect(overlay_hover.contents)
	)
	assert(overlay_hover.contents.value:find("OverlayName: Integer", 1, true), vim.inspect(overlay_hover))
	assert(not overlay_hover.contents.value:find("DiskName", 1, true), vim.inspect(overlay_hover))

	local type_definition = request(client, "textDocument/typeDefinition", {
		textDocument = { uri = main_uri },
		position = { line = 15, character = 4 },
	}, main)
	assert(#type_definition == 1, vim.inspect(type_definition))
	assert(type_definition[1].uri == vim.uri_from_fname(provider_path), vim.inspect(type_definition))
	assert_range(type_definition[1].range, 3, 2, 3, 9)

	local completion = request(client, "textDocument/completion", {
		textDocument = { uri = main_uri },
		position = { line = 18, character = 6 },
	}, main)
	assert(completion.isIncomplete == false, vim.inspect(completion))
	assert(vim.deep_equal(item_labels(completion.items), { "OverlayName" }), vim.inspect(completion.items))
	assert(completion.items[1].textEdit.newText == "OverlayName", vim.inspect(completion.items[1]))
	assert_range(completion.items[1].textEdit.range, 18, 2, 18, 6)
	assert(completion.items[1].textEdit.insert == nil, vim.inspect(completion.items[1]))
	assert(completion.items[1].textEdit.replace == nil, vim.inspect(completion.items[1]))

	local member_completion = request(client, "textDocument/completion", {
		textDocument = { uri = main_uri },
		position = { line = 19, character = 8 },
	}, main)
	assert(vim.deep_equal(item_labels(member_completion.items), { "Member" }), vim.inspect(member_completion.items))

	local call_line = vim.api.nvim_buf_get_lines(main, 20, 21, false)[1]
	local argument_prefix = "[1,2], "
	local argument_start = assert(call_line:find(argument_prefix, 1, true))
	local signature = request(client, "textDocument/signatureHelp", {
		textDocument = { uri = main_uri },
		position = { line = 20, character = argument_start - 1 + #argument_prefix },
	}, main)
	assert(signature.activeSignature == vim.NIL or signature.activeSignature == nil, vim.inspect(signature))
	assert(signature.activeParameter == 3, vim.inspect(signature))
	assert(#signature.signatures == 2, vim.inspect(signature.signatures))
	assert(
		signature.signatures[1].label == "procedure PublicRoutine(A, B: Integer; C: string; D: Integer);",
		vim.inspect(signature.signatures)
	)
	assert(#signature.signatures[1].parameters == 4, vim.inspect(signature.signatures[1]))
	local expected_parameter_offsets = { { 24, 25 }, { 27, 28 }, { 39, 40 }, { 50, 51 } }
	for index, parameter in ipairs(signature.signatures[1].parameters) do
		assert(vim.deep_equal(parameter.label, expected_parameter_offsets[index]), vim.inspect(signature.signatures[1]))
	end

	assert(vim.deep_equal(vim.fn.readfile(main_path), main_on_disk), "server changed Main.pas on disk")
	assert(vim.deep_equal(vim.fn.readfile(provider_path), provider_on_disk), "server changed Provider.pas on disk")
	assert(vim.bo[main].modified, "assistance requests must not save the overlay")
	client:stop()
	assert(
		vim.wait(REQUEST_TIMEOUT, function()
			return client:is_stopped()
		end),
		"server did not shut down"
	)
	io.stdout:write("NEOVIM_ASSISTANCE_OK\n")
end

local ok, error_message = xpcall(run, debug.traceback)
if not ok then
	io.stderr:write(tostring(error_message) .. "\n")
	vim.cmd("cquit 1")
else
	vim.cmd("qa!")
end
