import { Action, ActionPanel, closeMainWindow, Detail, Icon, Keyboard, List, open, showToast, Toast } from "@vicinae/api";
import { useEffect, useRef, useState } from "react";
import {ChildProcessWithoutNullStreams, spawn} from "node:child_process"
import readline, { createInterface } from "node:readline"
import { stat, statSync } from "node:fs";

export default function SQSearch() {
	const [search_text, set_search_text] = useState("")
	const [matches, set_matches] = useState<string[]>([])

	const proc_ref = useRef<ChildProcessWithoutNullStreams | null>(null)
	const active_query_ref = useRef('')
	const latest_query_ref = useRef('')

	useEffect(() => {
		const instance = spawn('sqsearch', ['query'])
		proc_ref.current = instance

		const reader = readline.createInterface(instance.stdout)

		reader.on('line', line => {
			if (line.startsWith('BEGIN')) {
				active_query_ref.current = line.slice('BEGIN '.length)
				return
			}
			if (line.startsWith('END')) {
				active_query_ref.current = ''
				return
			}
			if (line.startsWith('ERROR')) {
				console.log(line)
				return
			}
			if (active_query_ref.current !== latest_query_ref.current) return

			const path = line.slice('ITEM '.length)
			set_matches(prev => [...prev, path])
		})

		return () => {
			instance.kill()
			reader.close()
		}
	}, [])

	useEffect(() => {
		latest_query_ref.current = search_text

		if (!proc_ref.current) return
		set_matches([])
		console.log(`Search: ${search_text}`)
		if (search_text === '') return

		proc_ref.current.stdin.write(search_text + '\n')
	}, [search_text])

	return (
	<List
		searchBarPlaceholder="Search files..."
		onSearchTextChange={set_search_text}
		>

	{matches.map((path, index) => (file_panel(path, `${path}-${index}`)))}

	</List>
	);
}

function get_icon(path: string): Icon {
	// FIXME
	return Icon.Folder
}

function file_panel(path: string, key: string) {
	return  (
	<List.Item
		title={path}
		key={key}
		icon={get_icon(path)}
		actions={
		<ActionPanel>

		<Action title='Open' shortcut={'open'} onAction={() => {open(path); closeMainWindow()}}/>
		<Action.ShowInFinder path={path}/>
		<Action.CopyToClipboard title='Copy Path' shortcut={'copy-path'} content={path}/>

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
