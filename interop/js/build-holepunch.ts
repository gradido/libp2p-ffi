/** Compiles holepunch-node.ts into one executable for interop/holepunch. */
import { quicBinding } from './quic-binding-plugin.ts'

const outfile = process.argv[2] ?? '../holepunch/bin/holepunch-js'
const result = await Bun.build({
  entrypoints: ['./holepunch-node.ts'],
  compile: { outfile },
  target: 'bun',
  plugins: [quicBinding],
})
if (!result.success) {
  for (const log of result.logs) console.error(log)
  process.exit(1)
}
console.log(`built ${outfile}`)
