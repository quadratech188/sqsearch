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

	return [k, default_count, prefs['prefix'], prefs['db']]
}

const [k, default_count, prefix, db] = load_prefs()

type HistoryData = {
	score: number,
	// In seconds (Date.now() / 1000)
	last_updated: number
}

function get_score(data: HistoryData): number {
	return data.score * Math.exp(k * (Date.now() / 1000 - data.last_updated))
}

type SerializedHistory = [string, HistoryData][]

class History {
	by_path: Map<string, HistoryData>

	constructor(serialized: SerializedHistory) {
		this.by_path = new Map()

		for (const [k, v] of serialized) {
			this.by_path.set(k, v)
		}
	}

	serialize(): SerializedHistory {
		return Array.from(this.by_path.entries())
	}

	select(path: string) {
		let entry = this.by_path.get(path)

		this.by_path.set(path, {
			score: 1 + (entry !== undefined? get_score(entry): 0),
			last_updated: Date.now() / 1000
		})
	}
}

class Result {
	known_matches: [string, HistoryData][] = []
	matches: string[] = []
	history: History
	dirty: boolean = true

	constructor(history: History) {
		this.history = history
	}

	add(path: string) {
		this.dirty = true
		const entry = this.history.by_path.get(path)
		if (entry === undefined) {
			this.matches.push(path)
			return
		}
		this.known_matches.push([path, entry])
	}

	get_known_matches(): string[] {
		this.known_matches.sort((x, y) => {
			return get_score(y[1]) - get_score(x[1])
		})

		return this.known_matches.map(x => x[0])
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
		const instance = spawn('sqsearch', ['--db', db, 'query'])
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
		known: [] as string[],
		matches: [] as string[]
	})
	const sync = () => {
		if (!result_ref.current.dirty) return
		set_ui_results({
			known: result_ref.current.get_known_matches(),
			matches: [...result_ref.current.matches]
		})
		result_ref.current.dirty = false
	}
	useEffect(() => {
		const interval = setInterval(sync, 16)
		return () => {clearInterval(interval)}
	}, [])

	const query_ref = useRef('')
	const active_query_ref = useRef('')

	const [send_query, error] = use_sqsearch(line => {
		if (line.startsWith('BEGIN')) {
			active_query_ref.current = line.slice('BEGIN '.length)
			return
		}
		if (line.startsWith('END')) {
			active_query_ref.current = ''
			sync()
			return
		}
		if (line.startsWith('ERROR')) {
			console.log(line)
			return
		}
		if (active_query_ref.current !== query_ref.current) return

		const path = line.slice('ITEM '.length)
		result_ref.current.add(path)
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

	const select = (x: string) => {
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
			<FilePanel key={`${index}-${path}`}
				path={`${prefix}/${path}`}
				select={() => select(path)}
				more_results={() => set_count(x => 2 * x)}
				/>
			))}
		</List.Section>

		<List.Section title={`Results (${ui_results.matches.length})`}>
			{ui_results.matches.map((path, index) => (
			<FilePanel key={`${index}-${path}`}
				path={`${prefix}/${path}`}
				select={() => select(path)}
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
