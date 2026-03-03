import { Action, ActionPanel, Clipboard, closeMainWindow, Detail, Icon, Keyboard, List, LocalStorage, open, showInFileBrowser, showToast, Toast } from "@vicinae/api";
import { useEffect, useRef, useState, useSyncExternalStore } from "react";
import {ChildProcessWithoutNullStreams, spawn} from "node:child_process"
import readline, { createInterface } from "node:readline"
import { stat, statSync } from "node:fs";

const k = - 0.1

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

export default function SQSearch() {
	const [search_text, set_query] = useState("")
	const [count, set_count] = useState(512)

	const [known_matches, set_known_matches] = useState<string[]>([])
	const [matches, set_matches] = useState<string[]>([])

	const query_ref = useRef('')
	const proc_ref = useRef<ChildProcessWithoutNullStreams | null>(null)
	const history_ref = useRef(new History([]))
	const result_ref = useRef(new Result(history_ref.current))

	useEffect(() => {
		(async () => {
			let history: string | undefined
				= await LocalStorage.getItem('history')
			if (history === undefined) return

			history_ref.current = new History(JSON.parse(history))
		})()

		const instance = spawn('sqsearch', ['query'])
		proc_ref.current = instance

		const reader = readline.createInterface(instance.stdout)

		const sync = () => {
			if (!result_ref.current.dirty) return
			set_matches(result_ref.current.matches)
			set_known_matches(result_ref.current.get_known_matches())
			result_ref.current.dirty = false
		}

		const interval = setInterval(sync, 16)

		let active_query = ''
		reader.on('line', line => {
			if (line.startsWith('BEGIN')) {
				active_query = line.slice('BEGIN '.length)
				return
			}
			if (line.startsWith('END')) {
				active_query = ''
				sync()
				return
			}
			if (line.startsWith('ERROR')) {
				console.log(line)
				return
			}
			if (active_query !== query_ref.current) return

			const path = line.slice('ITEM '.length)
			result_ref.current.add(path)
		})

		return () => {
			clearInterval(interval)
			instance.kill()
		}
	}, [])

	useEffect(() => {
		result_ref.current = new Result(history_ref.current)

		if (search_text === '') return

		const query = `COUNT ${count} ${search_text}`
		query_ref.current = query
		console.log(`Query: ${query}`)

		if (!proc_ref.current) return
		proc_ref.current.stdin.write(query + '\n')
	}, [search_text, count])

	useEffect(() => {
		set_count(512)
	}, [search_text])

	const select = (x: string) => {
		console.log(x)
		history_ref.current.select(x)
		LocalStorage.setItem('history', JSON.stringify(history_ref.current.serialize()))
	}

	const more_results = () => {
		set_count(x => 2 * x)
	}

	return (
	<List
		searchBarPlaceholder="Search files..."
		onSearchTextChange={set_query}
		>
	
	<List.Section title={`Seen before (${result_ref.current.known_matches.length})`}>

	{known_matches.map((path, index) => (file_panel(path, `${path}-${index}`, select, more_results)))}

	</List.Section>

	<List.Section title={`Results (${result_ref.current.matches.length})`}>

	{matches.map((path, index) => (file_panel(path, `${path}-${index}`, select, more_results)))}

	</List.Section>

	</List>
	);
}

function get_icon(path: string): Icon {
	// FIXME
	return Icon.Folder
}

function file_panel(path: string, key: string, select: (x: string) => void, more_results: () => void) {
	return  (
	<List.Item
		title={path}
		key={key}
		icon={get_icon(path)}
		actions={
		<ActionPanel>

		<ActionPanel.Section>
			<Action icon={Icon.Folder}
				title='Open'
				shortcut={'open'}
				onAction={() => {
					select(path); open(path); closeMainWindow()
				}}/>
			<Action icon={Icon.Folder}
				title='Open Containing Folder' 
				onAction={() => {
					select(path); showInFileBrowser(path); closeMainWindow()
				}}/>
			<Action icon={Icon.CopyClipboard}
				title='Copy Path'
				shortcut={'copy-path'}
				onAction={() => {
					select(path); Clipboard.copy(path)
				}}/>
		</ActionPanel.Section>

		<ActionPanel.Section>
			<Action icon={Icon.RotateClockwise}
				title='More results'
				shortcut={'refresh'}
				onAction={more_results}
				/>
		</ActionPanel.Section>

		</ActionPanel>
		}
		detail={
			<List.Item.Detail
			metadata={
				<List.Item.Detail.Metadata>
					<List.Item.Detail.Metadata.Label title="Path" text={path}/>
				</List.Item.Detail.Metadata>
			}
			markdown=""
		/>
		}
	/>
	)
}
