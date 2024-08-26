import {
    transactions, 
    transactionScripts,
} from './schema.js'

export async function getTransactions(
    filter
) {
    let transactionRecords;

    try {
        if (filter === 'Uncomitted') {
            transactionRecords = await transactions.filter(tx => tx.commitHeight === undefined || tx.commitHeight === null).toArray();
        } else {
            transactionRecords = await transactions.toArray();
        }

        if (transactionRecords.length === 0) {
            return [];
        }

        const scriptHashes = transactionRecords.map(transactionRecord => {
            return transactionRecord.scriptHash
        });

        const scripts = await transactionScripts.where("scriptHash").anyOf(scriptHashes).toArray();

        // Create a map of scriptHash to script for quick lookup
        const scriptMap = new Map();
        scripts.forEach(script => {
            scriptMap.set(script.scriptHash, script.program);
        });

        const processedTransactions = await Promise.all(transactionRecords.map(async transactionRecord => {
            let scriptBase64 = null;

            if (transactionRecord.scriptHash) {
                const txScript = scriptMap.get(transactionRecord.scriptHash);

                if (txScript) {
                    let scriptArrayBuffer = await txScript.arrayBuffer();
                    let scriptArray = new Uint8Array(scriptArrayBuffer);
                    scriptBase64 = uint8ArrayToBase64(scriptArray);
                }
            }

            let outputNotesArrayBuffer = await transactionRecord.outputNotes.arrayBuffer();
            let outputNotesArray = new Uint8Array(outputNotesArrayBuffer);
            let outputNotesBase64 = uint8ArrayToBase64(outputNotesArray);

            transactionRecord.outputNotes = outputNotesBase64;

            let data = {
                id: transactionRecord.id,
                account_id: transactionRecord.accountId,
                init_account_state: transactionRecord.initAccountState,
                final_account_state: transactionRecord.finalAccountState,
                input_notes: transactionRecord.inputNotes,
                output_notes: transactionRecord.outputNotes,
                script_hash: transactionRecord.scriptHash ? transactionRecord.scriptHash : null,
                tx_script: scriptBase64,
                block_num: transactionRecord.blockNum,
                commit_height: transactionRecord.commitHeight ? transactionRecord.commitHeight : null
            }

            return data;
        }));

        return processedTransactions
    } catch {
        console.error("Failed to get transactions: ", err);
        throw err;
    }
}

export async function insertTransactionScript(
    scriptHash,
    txScript
) {
    try {
        // check if script hash already exists 
        let record = await transactionScripts.where("scriptHash").equals(scriptHash).first();

        if (record) {
            return;
        }

        if (!scriptHash) {
            throw new Error("Script hash must be provided");
        }

        let scriptHashArray = new Uint8Array(scriptHash);
        let scriptHashBase64 = uint8ArrayToBase64(scriptHashArray);
        let scriptBlob = null;

        if (txScript ) {
            scriptBlob = new Blob([new Uint8Array(txScript)]);
        }

        const data = {
            scriptHash: scriptHashBase64,
            script: scriptBlob
        }

        await transactionScripts.add(data);
    } catch (error) {
        // Check if the error is because the record already exists
        if (error.name === 'ConstraintError') {
        } else {
            // Re-throw the error if it's not a constraint error
            throw error;
        }
    }
}

export async function insertProvenTransactionData(
    transactionId,
    accountId,
    initAccountState,
    finalAccountState,
    inputNotes,
    outputNotes,
    scriptHash,
    blockNum,
    committed
) {
    try {
        let scriptHashBase64 = null;
        let outputNotesBlob = new Blob([new Uint8Array(outputNotes)]);
        if (scriptHash !== null) {
            let scriptHashArray = new Uint8Array(scriptHash);
            scriptHashBase64 = uint8ArrayToBase64(scriptHashArray);
        }

        const data = {
            id: transactionId,
            accountId: accountId,
            initAccountState: initAccountState,
            finalAccountState: finalAccountState,
            inputNotes: inputNotes,
            outputNotes: outputNotesBlob,
            scriptHash: scriptHashBase64,
            blockNum: blockNum,
            commitHeight: committed ? committed : null
        }

        await transactions.add(data);
    } catch (err) {
        console.error("Failed to insert proven transaction data: ", err);
        throw err;
    }
}

function uint8ArrayToBase64(bytes) {
    const binary = bytes.reduce((acc, byte) => acc + String.fromCharCode(byte), '');
    return btoa(binary);
}
