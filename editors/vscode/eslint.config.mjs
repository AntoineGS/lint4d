import tseslint from 'typescript-eslint';
export default tseslint.config(
  { ignores: ['dist/**', 'out/**', '.vscode-test/**', 'node_modules/**'] },
  ...tseslint.configs.recommended,
  {
    files: ['src/settings.ts', 'src/server.ts', 'src/protocol.ts', 'src/pickerLogic.ts', 'src/statusFormat.ts', 'src/dcuText.ts'],
    rules: { 'no-restricted-imports': ['error', { paths: [{ name: 'vscode', message: 'Pure module: keep vscode out so it stays unit-testable.' }, { name: 'vscode-languageclient', message: 'Pure module.' }, { name: 'vscode-languageclient/node', message: 'Pure module.' }] }] },
  },
);
