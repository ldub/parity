// Copyright 2015-2017 Parity Technologies (UK) Ltd.
// This file is part of Parity.

// Parity is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Parity is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Parity.  If not, see <http://www.gnu.org/licenses/>.

//! Wasm evm program runtime intstance

use std::sync::Arc;

use byteorder::{LittleEndian, ByteOrder};

use evm;

use parity_wasm::interpreter;
use util::{H256, Address};

use super::ptr::{WasmPtr, Error as PtrError};
use super::call_args::CallArgs;

/// Wasm runtime error
#[derive(Debug)]
pub enum Error {
	/// Storage error
	Storage,
	/// Allocator error
	Allocator,
	/// Invalid gas state during the call
	InvalidGasState,
	/// Memory access violation
	AccessViolation,
	/// Interpreter runtime error
	Interpreter(interpreter::Error),
}

impl From<interpreter::Error> for Error {
	fn from(err: interpreter::Error) -> Self {
		Error::Interpreter(err)
	}
}

impl From<PtrError> for Error {
	fn from(err: PtrError) -> Self {
		match err {
			PtrError::AccessViolation => Error::AccessViolation,
		}
	}
}

pub struct Runtime<'a> {
	gas_counter: u64,
	gas_limit: u64,
	dynamic_top: u32,
	ext: &'a mut evm::Ext,
	memory: Arc<interpreter::MemoryInstance>,
}

impl<'a> Runtime<'a> {
	pub fn with_params<'b>(
		ext: &'b mut evm::Ext,
		memory: Arc<interpreter::MemoryInstance>, 
		stack_space: u32, 
		gas_limit: u64,
	) -> Runtime<'b> {
		Runtime {
			gas_counter: 0,
			gas_limit: gas_limit,
			dynamic_top: stack_space,
			memory: memory,
			ext: ext,
		}
	}

	pub fn storage_write(&mut self, context: interpreter::CallerContext) 
		-> Result<Option<interpreter::RuntimeValue>, interpreter::Error>
	{
		let mut context = context;
		let val = self.pop_h256(&mut context)?;
		let key = self.pop_h256(&mut context)?;
		trace!(target: "wasm", "storage_write: value {} at @{}", &val, &key);

		// todo: return a runtime error contract can handle or as it is now - general failure?
		self.ext.set_storage(key, val)
			.map_err(|_| interpreter::Error::Trap("Storage update error".to_owned()))?;

		Ok(Some(0i32.into()))
	}

	pub fn storage_read(&mut self, context: interpreter::CallerContext) 
		-> Result<Option<interpreter::RuntimeValue>, interpreter::Error>
	{
		let mut context = context;
		let val_ptr = context.value_stack.pop_as::<i32>()?;
		let key = self.pop_h256(&mut context)?;		

		// todo: return a runtime error contract can handle or as it is now - general failure?
		let val = self.ext.storage_at(&key)
			.map_err(|_| interpreter::Error::Trap("Storage read error".to_owned()))?;

		self.memory.set(val_ptr as u32, &*val)?;

		Ok(Some(0.into()))
	}

	pub fn suicide(&mut self, context: interpreter::CallerContext) 
		-> Result<Option<interpreter::RuntimeValue>, interpreter::Error>
	{
		let mut context = context;
		let refund_address = self.pop_address(&mut context)?;	

		self.ext.suicide(&refund_address)
			.map_err(|_| interpreter::Error::Trap("Suicide error".to_owned()))?;

		Ok(None)
	}

	pub fn malloc(&mut self, context: interpreter::CallerContext) 
		-> Result<Option<interpreter::RuntimeValue>, interpreter::Error>
	{
		let amount = context.value_stack.pop_as::<i32>()? as u32;
		let previous_top = self.dynamic_top;
		self.dynamic_top = previous_top + amount;
		Ok(Some((previous_top as i32).into()))
	}

	pub fn alloc(&mut self, amount: u32) -> Result<u32, Error> {
		let previous_top = self.dynamic_top;
		self.dynamic_top = previous_top + amount;
		Ok(previous_top.into())
	}

	fn gas(&mut self, context: interpreter::CallerContext) 
		-> Result<Option<interpreter::RuntimeValue>, interpreter::Error> 
	{
		let prev = self.gas_counter;
		let update = context.value_stack.pop_as::<i32>()? as u64;
		if prev + update > self.gas_limit {
			// exceeds gas
			Err(interpreter::Error::Trap(format!("Gas exceeds limits of {}", self.gas_limit)))
		} else {
			self.gas_counter = prev + update;
			Ok(None)
		}
	}

	fn h256_at(&self, ptr: WasmPtr) -> Result<H256, interpreter::Error> {
		Ok(H256::from_slice(&ptr.slice(32, &*self.memory)
			.map_err(|_| interpreter::Error::Trap("Memory access violation".to_owned()))?
		))	
	}

	fn pop_h256(&self, context: &mut interpreter::CallerContext) -> Result<H256, interpreter::Error> {
		let ptr = WasmPtr::from_i32(context.value_stack.pop_as::<i32>()?)
			.map_err(|_| interpreter::Error::Trap("Memory access violation".to_owned()))?;
		self.h256_at(ptr)
	}

	fn address_at(&self, ptr: WasmPtr) -> Result<Address, interpreter::Error> {
		Ok(Address::from_slice(&ptr.slice(20, &*self.memory)
			.map_err(|_| interpreter::Error::Trap("Memory access violation".to_owned()))?
		))	
	}

	fn pop_address(&self, context: &mut interpreter::CallerContext) -> Result<Address, interpreter::Error> {
		let ptr = WasmPtr::from_i32(context.value_stack.pop_as::<i32>()?)
			.map_err(|_| interpreter::Error::Trap("Memory access violation".to_owned()))?;
		self.address_at(ptr)
	}

	fn user_trap(&mut self, _context: interpreter::CallerContext) 
		-> Result<Option<interpreter::RuntimeValue>, interpreter::Error> 
	{
		Err(interpreter::Error::Trap("unknown trap".to_owned()))
	}

	fn user_noop(&mut self, 
		_context: interpreter::CallerContext
	) -> Result<Option<interpreter::RuntimeValue>, interpreter::Error> {
		Ok(None)
	}

	pub fn write_descriptor(&mut self, call_args: CallArgs) -> Result<WasmPtr, Error> {
		let d_ptr = self.alloc(16)?;

		let args_len = call_args.len();
		let args_ptr = self.alloc(args_len)?;

		// write call descriptor
		// call descriptor is [args_ptr, args_len, return_ptr, return_len]
		//   all are 4 byte length, last 2 are zeroed
		let mut d_buf = [0u8; 16];
		LittleEndian::write_u32(&mut d_buf[0..4], args_ptr);
		LittleEndian::write_u32(&mut d_buf[4..8], args_len);
		self.memory.set(d_ptr, &d_buf)?;

		// write call args to memory
		self.memory.set(args_ptr, &call_args.address)?;
		self.memory.set(args_ptr+20, &call_args.sender)?;
		self.memory.set(args_ptr+40, &call_args.origin)?;
		self.memory.set(args_ptr+60, &call_args.value)?;
		self.memory.set(args_ptr+92, &call_args.data)?;
		
		Ok(d_ptr.into())
	}

	fn debug_log(&mut self, context: interpreter::CallerContext) 
			-> Result<Option<interpreter::RuntimeValue>, interpreter::Error> 
	{
		let msg_len = context.value_stack.pop_as::<i32>()? as u32;
		let msg_ptr = context.value_stack.pop_as::<i32>()? as u32;
		
		let msg = String::from_utf8(self.memory.get(msg_ptr, msg_len as usize)?)
			.map_err(|_| interpreter::Error::Trap("Debug log utf-8 decoding error".to_owned()))?;

		trace!(target: "wasm", "Contract debug message: {}", msg);

		Ok(None)
	}

	pub fn gas_left(&self) -> Result<u64, Error> {
		if self.gas_counter > self.gas_limit { return Err(Error::InvalidGasState); }
		Ok(self.gas_limit - self.gas_counter)
	}

	pub fn memory(&self) -> &interpreter::MemoryInstance {
		&*self.memory
	}
}

impl<'a> interpreter::UserFunctionExecutor for Runtime<'a> {
	fn execute(&mut self, name: &str, context: interpreter::CallerContext) 
		-> Result<Option<interpreter::RuntimeValue>, interpreter::Error>
	{
		match name {
			"_malloc" => {
				self.malloc(context)
			},
			"_free" => {
				// Since it is arena allocator, free does nothing
				// todo: update if changed
				self.user_noop(context)
			},
			"_storage_read" => {
				self.storage_read(context)
			},
			"_storage_write" => {
				self.storage_write(context)
			},
			"_suicide" => {
				self.suicide(context)
			},
			"_debug" => {
				self.debug_log(context)
			},
			"gas" => {
				self.gas(context)
			},
			_ => {
				trace!("Unknown env func: '{}'", name);
				self.user_trap(context)
			}
		}
	}
}