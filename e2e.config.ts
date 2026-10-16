import type { E2EConfig } from 'e2e';
import { web } from '@e2e-dev/web';

export default {
  tests: 'tests/**/*.e2e.ts',
  targets: [
    {
      engine: web(),
      app: {
        url: 'http://127.0.0.1:1420',
        command: {
          executable: 'npm',
          args: ['run', 'dev', '--', '--host', '127.0.0.1', '--port', '1420', '--strictPort'],
          log: '.e2e/logs/vite.log',
        },
      },
    },
  ],
} satisfies E2EConfig;
