import js from '@eslint/js'
import globals from 'globals'
import reactHooks from 'eslint-plugin-react-hooks'
import reactRefresh from 'eslint-plugin-react-refresh'
import tseslint from 'typescript-eslint'
import { defineConfig, globalIgnores } from 'eslint/config'

export default defineConfig([
  globalIgnores(['dist']),
  {
    files: ['**/*.{ts,tsx}'],
    extends: [
      js.configs.recommended,
      tseslint.configs.recommended,
      reactHooks.configs.flat.recommended,
      reactRefresh.configs.vite,
    ],
    languageOptions: {
      ecmaVersion: 2020,
      globals: globals.browser,
    },
  },
  {
    // shadcn/ui and AI Elements are registry-managed source. Their public
    // modules intentionally co-locate helpers with components, and a few
    // upstream animation/highlighting patterns trip React's opt-in compiler
    // diagnostics even though this app does not compile them with React
    // Compiler. Keep the rest of the recommended rules active.
    files: ['src/components/ui/**/*.{ts,tsx}', 'src/components/ai-elements/**/*.{ts,tsx}'],
    rules: {
      '@typescript-eslint/no-unused-vars': 'off',
      'react-hooks/refs': 'off',
      'react-hooks/static-components': 'off',
      'react-refresh/only-export-components': 'off',
    },
  },
])
