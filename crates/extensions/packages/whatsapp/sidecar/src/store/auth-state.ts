// Baileys AuthenticationState backed by the encrypted store.
//
// Mirrors Baileys' own useMultiFileAuthState (one record per signal key,
// BufferJSON serialisation, AppStateSyncKeyData revived through protobuf), but
// every record is encrypted and named by HMAC. The socket wraps `keys` with
// addTransactionCapability itself (Baileys Socket/socket.ts), so the caller
// only adds makeCacheableSignalKeyStore.
import { BufferJSON, initAuthCreds, proto } from 'baileys'
import type { AuthenticationCreds, AuthenticationState, SignalDataTypeMap, SignalKeyStore } from 'baileys'
import type { EncryptedStore } from './encrypted-store.js'

export const CREDS_TYPE = 'creds'
const CREDS_ID = 'creds'

/** Record type for one Baileys signal-data category. */
export function signalType(category: string): string {
	return `signal:${category}`
}

export function encodeJson(value: unknown): Buffer {
	return Buffer.from(JSON.stringify(value, BufferJSON.replacer), 'utf8')
}

export function decodeJson<T>(bytes: Buffer): T {
	return JSON.parse(bytes.toString('utf8'), BufferJSON.reviver) as T
}

export async function getJson<T>(store: EncryptedStore, type: string, id: string): Promise<T | undefined> {
	const bytes = await store.getBytes(type, id)
	return bytes === undefined ? undefined : decodeJson<T>(bytes)
}

export async function putJson(store: EncryptedStore, type: string, id: string, value: unknown): Promise<void> {
	await store.putBytes(type, id, encodeJson(value))
}

export interface EncryptedAuthState {
	state: AuthenticationState
	saveCreds: () => Promise<void>
	/** True when creds were loaded from the store (not freshly generated). */
	existing: boolean
}

export async function loadEncryptedAuthState(store: EncryptedStore): Promise<EncryptedAuthState> {
	const stored = await getJson<AuthenticationCreds>(store, CREDS_TYPE, CREDS_ID)
	const creds: AuthenticationCreds = stored ?? initAuthCreds()

	const keys: SignalKeyStore = {
		get: async (type, ids) => {
			const data: { [id: string]: SignalDataTypeMap[typeof type] } = {}
			await Promise.all(
				ids.map(async id => {
					let value = await getJson<SignalDataTypeMap[typeof type]>(store, signalType(type), id)
					if (value === undefined || value === null) return
					if (type === 'app-state-sync-key') {
						value = proto.Message.AppStateSyncKeyData.fromObject(
							value as object
						) as unknown as SignalDataTypeMap[typeof type]
					}
					data[id] = value
				})
			)
			return data
		},
		set: async data => {
			const tasks: Promise<void>[] = []
			for (const category of Object.keys(data) as Array<keyof SignalDataTypeMap>) {
				const entries = data[category]
				if (!entries) continue
				for (const [id, value] of Object.entries(entries)) {
					tasks.push(
						value === null || value === undefined
							? store.delete(signalType(category), id)
							: putJson(store, signalType(category), id, value)
					)
				}
			}
			await Promise.all(tasks)
		}
	}

	return {
		state: { creds, keys },
		saveCreds: () => putJson(store, CREDS_TYPE, CREDS_ID, creds),
		existing: stored !== undefined
	}
}

/** Every Baileys signal-data category this version knows about. */
export const SIGNAL_CATEGORIES: ReadonlyArray<keyof SignalDataTypeMap> = [
	'pre-key',
	'session',
	'sender-key',
	'sender-key-memory',
	'app-state-sync-key',
	'app-state-sync-version',
	'lid-mapping',
	'device-list',
	'tctoken',
	'identity-key'
]
