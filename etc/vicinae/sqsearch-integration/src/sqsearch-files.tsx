import { Action, ActionPanel, Clipboard, closeMainWindow, Detail, getPreferenceValues, Icon, List, LocalStorage, open, showInFileBrowser} from "@vicinae/api";
import { useLocalStorage } from "@raycast/utils"
import { useCallback, useEffect, useRef, useState} from "react";
import {ChildProcessWithoutNullStreams, spawn} from "node:child_process"
import readline from "node:readline"

function load_prefs() {
	const prefs = getPreferenceValues()
	const half_life = Number(prefs['half-life']) * 86400
	// e^kt = 1/2
	// kt = - ln 2
	// k = - (ln 2) / t
	const k = - Math.log(2) / half_life
	const default_count = Number(prefs['default-count'])
	const history_len = Number(prefs['history-len'])

	return [k, default_count, history_len, prefs['prefix'], prefs['db']]
}

const [k, default_count, history_len, prefix, db] = load_prefs()

type HistoryData = {
	score: number,
	// In seconds (Date.now() / 1000)
	last_updated: number
}

function get_score(data: HistoryData): number {
       return data.score * Math.exp(k * (Date.now() / 1000 - data.last_updated))
}

type SerializedHistory = [number, HistoryData][]

class History {
	by_id: Map<number, HistoryData>

	constructor(serialized: SerializedHistory) {
		console.log(serialized.length)
		this.by_id = new Map()

		for (const [k, v] of serialized) {
			this.by_id.set(k, v)
		}
	}
	serialize(): SerializedHistory {
		let serialized = Array.from(this.by_id.entries())
		serialized.sort((x, y) => {return get_score(y[1]) - get_score(x[1])})
		return serialized.slice(0, history_len)
	}
	score(id: number) {
		let entry = this.by_id.get(id)
		if (entry === undefined) return 0

		return get_score(entry)
	}
	select(id: number) {

		this.by_id.set(id, {
			score: 1 + this.score(id),
			last_updated: Date.now() / 1000
		})
	}
}

class Result {
	known_matches: [number, string][] = []
	matches: [number, string][] = []
	history: History
	dirty: boolean = true

	constructor(history: History) {
		this.history = history
	}

	add(id: number, path: string) {
		this.dirty = true

		if (this.history.by_id.has(id)) {
			this.known_matches.push([id, path])
		} else {
			this.matches.push([id, path])
		}
	}
	sort_known_matches() {
		this.known_matches.sort((x, y) => {
			return this.history.score(y[0]) - this.history.score(x[0])
		})
	}
	clear() {
		this.dirty = true
		this.matches = []
	}
}

function use_sqsearch(on_line: (line: string) => void):
[(line: string) => void, null | string] {
	const proc_ref = useRef<ChildProcessWithoutNullStreams | null>(null)

	const callback_ref = useRef(on_line)
	useEffect(() => {callback_ref.current = on_line}, [on_line])

	const [error, set_error] = useState<null | string>(null)

	useEffect(() => {
		const instance = spawn('sqsearch', ['--db', db, 'query', '--format', 'jsonl'])
		proc_ref.current = instance

		let stderr_buffer = ''

		instance.on('error', err => {})

		instance.stderr.on('data', chunk => {
			stderr_buffer += chunk.toString()
		})

		const reader = readline.createInterface(instance.stdout)
		reader.on('line', callback_ref.current)

		instance.on('close', code => {
			if (code === -2) {
				set_error(`### Failed to find \`sqsearch\`! \
					get it [here](https://github.com/quadratech188/sqsearch)`)
			}
			else if (code !== 0) {
				set_error(`\`\`\` Exit Code ${code}\n${stderr_buffer}\n\`\`\``)
			}
		})

		return () => {
			instance.kill()
		}

	}, [])

	const send = useCallback((line: string) => {
		proc_ref.current?.stdin.write(line)
	}, [])

	return [send, error]
}


export default function SQSearch() {
	const [search_text, set_query] = useState("")
	const [count, set_count] = useState(default_count)

	const [ui_results, set_ui_results] = useState({
		known: [] as [number, string][],
		matches: [] as [number, string][]
	})
	const sync = () => {
		if (!result_ref.current.dirty) return
		result_ref.current.sort_known_matches()
		set_ui_results({
			known: [...result_ref.current.known_matches],
			matches: [...result_ref.current.matches]
		})
		result_ref.current.dirty = false
	}
	useEffect(() => {
		let interval: NodeJS.Timeout | undefined = undefined
		const timeout = setTimeout(() => {
			interval = setInterval(sync, 33)
		}, 100)
		return () => {
			clearTimeout(timeout)
			if (interval) clearInterval(interval)
		}
	}, [search_text])

	const query_ref = useRef('')
	const active_query_ref = useRef('')

	const [send_query, error] = use_sqsearch(line => {
		const parsed = JSON.parse(line)

		if (parsed.type == "Begin") {
			active_query_ref.current = parsed.value
			return
		}
		if (parsed.type == "End") {
			active_query_ref.current = ''
			sync()
			return
		}
		if (parsed.type == "Error") {
			console.log(parsed.value)
			return
		}
		if (active_query_ref.current !== query_ref.current) return

		result_ref.current.add(parsed.value.id, parsed.value.path)
	})

	const {value: serialized_history, setValue: save_history, isLoading: loading}
		= useLocalStorage<SerializedHistory>("history", [])
	const history = new History(serialized_history || [])

	const result_ref = useRef(new Result(history))

	useEffect(() => {
		if (loading) return
		result_ref.current = new Result(history)

		if (search_text === '') return

		const query = `COUNT ${count} ${search_text}`
		query_ref.current = query
		console.log(`Query: ${query}`)

		send_query(query + '\n')
	}, [search_text, count, loading])

	useEffect(() => {
		set_count(default_count)
	}, [search_text])

	const select = (x: number) => {
		history.select(x)
		save_history(history.serialize())
	}

	if (error) {
		return <Detail
			markdown={`\n# \`sqsearch\` encountered an error!\n${error}`}

			actions={<ActionPanel>
				<Action.OpenInBrowser
					shortcut={'open'}
					title='Open Link'
					url={'https://github.com/quadratech188/sqsearch'}
					/>
			</ActionPanel>}
		/>
	}

	return (
	<List searchBarPlaceholder="Search files..."
		onSearchTextChange={set_query}
		isLoading={loading}>
		<List.Section title={`Seen before (${ui_results.known.length})`}>
			{ui_results.known.map((path, index) => (
			<FilePanel key={path[0]}
				path={`${prefix}/${path[1]}`}
				select={() => select(path[0])}
				more_results={() => set_count(x => 2 * x)}
				/>
			))}
		</List.Section>

		<List.Section title={`Results (${ui_results.matches.length})`}>
			{ui_results.matches.map((path, index) => (
			<FilePanel key={path[0]}
				path={`${prefix}/${path[1]}`}
				select={() => select(path[0])}
				more_results={() => set_count(x => 2 * x)}
				/>
			))}
		</List.Section>
	</List>
	);
}

function get_icon(path: string): Icon {
	// FIXME
	return Icon.Folder
}

interface FilePanelProps {
	path: string
	select: () => void
	more_results: () => void
}
function FilePanel({path, select, more_results}: FilePanelProps) {
	const segments = path.split('/')
	const filename = segments[segments.length - 1]
	return (
	<List.Item
		title={path}
		subtitle={filename}
		icon={get_icon(path)}
		dragContent={{file: path}}
		actions={
			<ActionPanel>
				<ActionPanel.Section>
					<Action icon={Icon.Folder}
						title='Open'
						shortcut={'open'}
						onAction={() => {
							select();
							open(path);
							closeMainWindow()
						}}/>
					<Action icon={Icon.Folder}
						title='Open Containing Folder'
						onAction={() => {
							select();
							showInFileBrowser(path);
							closeMainWindow()
						}}/>
					<Action icon={Icon.CopyClipboard}
						title='Copy Path'
						shortcut={'copy-path'}
						onAction={() => {
							select();
							Clipboard.copy(path)
						}}/>
				</ActionPanel.Section>

				<ActionPanel.Section>
					<Action icon={Icon.RotateClockwise}
						title='More results'
						shortcut={'refresh'}
						onAction={more_results}/>
				</ActionPanel.Section>
			</ActionPanel>
		}/>
	)
}
