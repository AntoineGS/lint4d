program FlowExceptionHandlerVariable;

type
  TTestException = class
  end;

procedure Demo;
begin
  try
  except
    on Error: TTestException do begin end;
  end;
end;

begin
  Demo;
end.
