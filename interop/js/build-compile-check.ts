/** Builds compile-check.ts into one executable, the way gradido2's bundle is built. */
import { quicBinding } from './quic-binding-plugin.ts'

const result = await Bun.build({
  entrypoints: ['./compile-check.ts'],
  compile: { outfile: './dist/compile-check' },
  target: 'bun',
  plugins: [quicBinding],
})
if (!result.success) {
  for (const log of result.logs) console.error(log)
  process.exit(1)
}
console.log('built dist/compile-check')
