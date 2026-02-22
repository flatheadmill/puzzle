function close_claude {
    exec {claude_in}&>-
}

function close_everything {
    typeset closer pid
    for closer pid in "${(@)corpocs}"; do
        $closer $pid
        wait $pid
    done
}

function abend {
    jo 'type=error' message=${1:-} >> $tmp/puzzle.jsonl
    close_everything
    tail_and_die
}

function tail_and_die {
    printf 'error: %s\n' "${1}" >> $tmp/puzzle.err
    tail $tmp/puzzle.jsonl $tmp/puzzle.err $tmp/stderr.txt $tmp/stdout.jsonl
    exit 1
}

function resume {
    {
        if ! whence jo > /dev/null; then
            printf '{"type":"error","message","jo is not installed"}' >> $tmp/puzzle.jsonl
            tail_and_die
        fi

        # Does not matter if resume is an sid or a file.
        typeset session_id claude_projects

        # This is an intialization area where we can abend quickly before we
        # find out that our environment is ill suited for our objectives.

        # Basic indications that the host is correctly configured.
        [[ -d $HOME ]] || abend '$HOME not defiend'
        [[ -d $HOME/.claude/projects ]] || abend '~/.claude/projects does not exist'
        whence fswatch > /dev/null || abend 'fswatch is not installed'
        [[ -e $HOME/.local/bin/claude ]] || abend 'claude is not installed'

        # Create our bogus project directory.
        claude_projects=$HOME/.claude/projects
        claude_projects=${claude_projects:A}

        # Need to be able to do these basics.
        mkdir -p $claude_projects/-tmp-puzzle-timeout/ || abend 'cannot create /tmp/puzzle/timeout project'

        # Figure things out with the payload, extract the stuff.

        # resume - sid or json payload, if json payload write to tmp file.
        # additional_arguments - like it sounds
        # kickoff - first message

        typeset json
        read -r json
        typeset -A config
        config=( "${(@QA)${(z)$(
            jq -r '
                [
                    "session_id", (.session_id // ""),
                    "yolo", if .yolo then 1 else 0 end,
                    "kickoff", .kickoff
                ] | @sh
            ' <<< "$json"
        )}}" )
        typeset additional_arguments=()
        if (( yolo )); then
            additional_arguments+=( --dangerously-skip-permissions )
        fi
        print here
        print -r "${(@kv)config}"
        print here
        exit


        integer claude_in claude_out
        coproc {
            coproc :
            $HOME/.local/bin/claude \
                --replay-user-messages \
                --input-format stream-json \
                --output-format stream-json \
                --add-dir ~/code/ \
                --verbose \
                --print \
                --resume "$resume" \
                "${(@)additonal_arguments}" 2> $tmp/stderr.err
        }
        coprocs+=( close_claude ${!} )
        exec {claude_in}>&p
        exec {claude_out}<&p
        coproc :

        # Slow read -r, but we only want one line.
        while read -r line; do
            printf '%s\n' "$line" > $tmp/stdout.jsonl
            if ! session_id=$(jq -r '.session_id // ""' <<< "$line"); then
                session_id=
            fi
            break
        done <&{claude_out}

        # If we cannot get our hands on a session_id, that's no good at all.
        if [[ -z $session_id ]]; then
            kill $claude_pid
            abend 'unable to obtain `claude` process id'
        fi

        # Send standard out to a file. Note that this is not garaunteed to flush
        # the final message, we have a race where we might kill our multi-tail
        # before it gets a chance to tail the last line of stdout.jsonl, but the
        # apprearance of the transcript replay will make it clear that no more
        # output from `claude` will be available .
        coproc {
            coproc :
            cat <&{claude_out} >> $tmp/stdout.jsonl
            jo -a flushed >> $tmp/stdout.jsonl
        }
        coprocs+=( true ${!} )

        # Watch for the transcript creation.
        integer fswatch_out fswatch_pid
        coproc {
            coproc :
            fswatch --event Created $claude_projects
        }
        fswatch_pid=${!}
        exec {fswatch_out}<&p
        coproc :

        # This will trigger the creation of the transcript.
        jo 'type=user' message="$(jo 'role=user' "content=$kickoff")" | >&${claude_in}

        integer timeout_in timeout_pid
        coproc {
            typeset line
            coproc :
            read -r -t 15 line
            if [[ $line != cancel ]]; then
                touch $claude_projects/-tmp-puzzle-timeout/$session_id.jsonl
            fi
        }
        timeout_pid=${!}
        exec {timeout_in}>&p
        coproc :

        # Slow read -r, but we probably only read one line.
        typeset transcript
        while read -r line; do
            if [[ $line = */$session_id.jsonl ]]; then
                kill $fswatch_pid
                if [[ ${line:h:t} = '-tmp-puzzle-timeout' ]]; then
                    abend 'timeout waiting for transcript'
                else
                    printf 'cancel\n' >&${timeout_in}
                    transcript=$line
                    coproc tail -n +1 -f $tmp/puzzle.jsonl $tmp/stderr.err $tmp/puzzle.err $tmp/stdout.jsonl $transcript
                    coprocs+=( kill ${!} )
                fi
                break
            fi
        done <&{fswatch_out}

        >&{claude_in} < <(cat)
        claude_in=0

        printf '==> terminated <=='

        tail -v $trasnscript
    } always {
        [[ -d $tmp ]] && rm -rf $tmp
    }
}

function {
    typeset coprocs=()
    typeset tmp=$(mktemp -d)
    {
        touch $tmp/puzzle.jsonl $tmp/stderr.err $tmp/puzzle.err $tmp/stdout.jsonl $tmp/transcript.jsonl
        resume 2> $tmp/puzzle.err
    } always {
        [[ -d $tmp ]] && rm -rf $tmp
    }
}
